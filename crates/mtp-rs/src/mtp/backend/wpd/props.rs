//! WPD property-bag helpers, string marshalling, and HRESULT → neutral error mapping.
//!
//! All the small, mostly-`unsafe` glue between windows-rs and our neutral [`crate::mtp`] types lives
//! here so [`super::com`] reads top-to-bottom. The typed property-bag getters
//! (`GetStringValue`/`GetGuidValue`/`GetUnsignedLargeIntegerValue`) keep us out of raw `PROPVARIANT`
//! handling, as proven in the Phase 0 spike.

use super::ole_date::{datetime_to_ole_date, device_wall_clock, ole_date_to_datetime};
use crate::mtp::error::map_response_code;
use crate::mtp::{DateTime, Error, ObjectFormat, ObjectHandle, ObjectInfo, StorageId, UtcOffset};
use crate::ptp::ResponseCode;
use std::ffi::c_void;

use windows::core::{Error as WinError, GUID, PWSTR};
use windows::Win32::Devices::PortableDevices::{
    IPortableDeviceProperties, IPortableDeviceValues, WPD_CONTENT_TYPE_FOLDER,
    WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT, WPD_OBJECT_CONTENT_TYPE, WPD_OBJECT_DATE_CREATED,
    WPD_OBJECT_DATE_MODIFIED, WPD_OBJECT_NAME, WPD_OBJECT_ORIGINAL_FILE_NAME, WPD_OBJECT_SIZE,
};
use windows::Win32::Foundation::{PROPERTYKEY, SYSTEMTIME};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Time::SystemTimeToTzSpecificLocalTime;
use windows::Win32::System::Variant::VT_DATE;

/// Read a WPD date property (`VT_DATE`) as a zoneless [`DateTime`], like the zoneless dates the
/// USB backend reads from `ObjectInfo`. `None` when the device doesn't report it or it isn't a
/// `VT_DATE`.
///
/// Zoneless is what the hardware shows, not an assumption: the Windows MTP driver hands the
/// device's wall-clock time through without converting it. On a Xiaomi POCO X7 Pro (Windows 11,
/// PR #41) `IMG_20260809_190833.jpg` read back as 19:08:33, where a local↔UTC conversion would
/// have shifted it by the PC's offset. A Pixel 9 Pro XL under Windows 10 agrees (#67): a file the
/// phone stores at 10:23 local read back as 10:23 with the PC in Central European time and in
/// Pacific time alike. A phone that reports a different hour from the USB backend is a reason to
/// revisit this.
///
/// # Safety
/// `v` must be a live `IPortableDeviceValues`.
pub(crate) unsafe fn read_date(v: &IPortableDeviceValues, key: &PROPERTYKEY) -> Option<DateTime> {
    let mut pv = v.GetValue(key).ok()?;
    let inner = &pv.Anonymous.Anonymous;
    let days = (inner.vt == VT_DATE).then(|| inner.Anonymous.date);
    // A VT_DATE owns nothing, but a device could store any type under the key.
    let _ = PropVariantClear(&mut pv);
    ole_date_to_datetime(days?)
}

/// Write an upload's date into a WPD property bag as a `VT_DATE`.
///
/// The mirror of [`read_date`]: a `VT_DATE` carries no zone, and the reading side shows the
/// Windows MTP driver passes the device's wall-clock time through unconverted, so we write the
/// wall-clock time we want the phone to show and expect the driver to send it as a zoneless
/// `DateModified`, which Android reads as phone-local (AOSP `MtpUtils.cpp` `parseDateTime`).
/// Microsoft's property docs ("Object Properties", `PortableDevice.h`) say only "the date and
/// time the object was modified on the device", with no zone.
///
/// A zoneless value goes out as is. A value with an offset names an instant, so it goes out as
/// that instant in the PC's time zone (`SystemTimeToTzSpecificLocalTime`, which applies the DST
/// rule in force at that date), the best stand-in we have for the phone's zone. That matches
/// Android over USB for the `Z` values `NewObjectInfo::with_modified` recommends, which Android
/// converts to phone-local itself. Should the PC's zone not resolve, the value's own wall-clock
/// fields go out.
///
/// Hardware-verified on a Pixel 9 Pro XL under Windows 10 (#67): with PC and phone in the same
/// zone, a `Z` date lands on the phone's file system at the exact instant. With the PC in another
/// zone it lands at the PC's wall-clock time, so the driver really does pass `VT_DATE` through
/// unconverted, and a phone in a different zone ends up off by the zone difference.
///
/// # Safety
/// `bag` must be a live `IPortableDeviceValues`.
pub(crate) unsafe fn write_date(
    bag: &IPortableDeviceValues,
    key: &PROPERTYKEY,
    date: DateTime,
) -> Result<(), Error> {
    let wall = device_wall_clock(date, pc_local_offset_at);
    let days = datetime_to_ole_date(wall).ok_or_else(|| {
        Error::invalid_data(format!(
            "can't send {:04}-{:02}-{:02} {:02}:{:02}:{:02} as a Windows date (VT_DATE)",
            date.year, date.month, date.day, date.hour, date.minute, date.second
        ))
    })?;
    let mut pv = PROPVARIANT::default();
    let inner = &mut *pv.Anonymous.Anonymous;
    inner.vt = VT_DATE;
    inner.Anonymous.date = days;
    // The bag copies the value; a VT_DATE owns nothing, so there's nothing to clear.
    bag.SetValue(key, &pv).map_err(map_hresult)
}

/// The PC's UTC offset at the instant `secs` (Unix seconds), DST included.
fn pc_local_offset_at(secs: i64) -> Option<UtcOffset> {
    let utc = DateTime::from_unix_seconds(secs, UtcOffset::UTC)?;
    let utc_st = SYSTEMTIME {
        wYear: utc.year,
        wMonth: utc.month.into(),
        wDayOfWeek: 0,
        wDay: utc.day.into(),
        wHour: utc.hour.into(),
        wMinute: utc.minute.into(),
        wSecond: utc.second.into(),
        wMilliseconds: 0,
    };
    let mut local_st = SYSTEMTIME::default();
    // SAFETY: both pointers are to live, properly aligned SYSTEMTIMEs; `None` means the current
    // time zone.
    unsafe { SystemTimeToTzSpecificLocalTime(None, &utc_st, &mut local_st) }.ok()?;
    let local = DateTime::new(
        local_st.wYear,
        u8::try_from(local_st.wMonth).ok()?,
        u8::try_from(local_st.wDay).ok()?,
        u8::try_from(local_st.wHour).ok()?,
        u8::try_from(local_st.wMinute).ok()?,
        u8::try_from(local_st.wSecond).ok()?,
    )?;
    let diff = local.to_unix_seconds_with_fallback(UtcOffset::UTC)? - secs;
    UtcOffset::from_minutes(i16::try_from(diff / 60).ok()?)
}

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

    if let Some(code) = mtp_response_code(e.code().0) {
        return map_response_code(code);
    }
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

/// The MTP response code inside an `HRESULT`, when Windows passed one through.
///
/// The WPD driver hands a device's MTP response back as a `FACILITY_ITF` failure (`0x8004_xxxx`)
/// with the response code in the low 16 bits: a deleted object's handle on a POCO X7 Pro under
/// Windows 11 came back as `0x8004_2009`, MTP `InvalidObjectHandle` (#41). Only the MTP
/// response-code ranges count (`0x2xxx` standard, `0xAxxx` MTP extensions), since other
/// `FACILITY_ITF` codes are unrelated COM errors (`0x8004_0154` is `REGDB_E_CLASSNOTREG`).
fn mtp_response_code(hresult: i32) -> Option<ResponseCode> {
    let hresult = hresult as u32;
    if hresult & 0xFFFF_0000 != 0x8004_0000 {
        return None;
    }
    let code = (hresult & 0xFFFF) as u16;
    matches!(code >> 12, 0x2 | 0xA).then(|| ResponseCode::from(code))
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
/// function never touches the (mutex-guarded) map while doing COM I/O. Datetimes come from
/// `WPD_OBJECT_DATE_CREATED`/`_MODIFIED` and are `None` when the device doesn't report them.
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

    let (filename, size, folder, created, modified) = match vals {
        Ok(v) => {
            let (created, modified) = (
                read_date(&v, &WPD_OBJECT_DATE_CREATED),
                read_date(&v, &WPD_OBJECT_DATE_MODIFIED),
            );
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
            (filename, size, folder, created, modified)
        }
        Err(_) => (String::new(), 0, false, None, None),
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
        created,
        modified,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hresult(code: u32) -> Error {
        map_hresult(WinError::from_hresult(windows::core::HRESULT(code as i32)))
    }

    #[test]
    fn mtp_response_code_hresults_map_like_the_usb_backend() {
        // A deleted object's handle on a POCO X7 Pro under Windows 11 (#41): MTP 0x2009
        // InvalidObjectHandle, wrapped as FACILITY_ITF.
        assert!(matches!(hresult(0x8004_2009), Error::StaleHandle));
        assert!(matches!(hresult(0x8004_201A), Error::StaleHandle)); // InvalidParentObject
        assert!(matches!(hresult(0x8004_2008), Error::NotFound)); // InvalidStorageId
        assert!(matches!(hresult(0x8004_200C), Error::StorageFull)); // StoreFull
        assert!(matches!(hresult(0x8004_2019), Error::Busy)); // DeviceBusy
        assert!(matches!(hresult(0x8004_2005), Error::Unsupported)); // OperationNotSupported
    }

    #[test]
    fn other_facility_itf_hresults_stay_other() {
        // REGDB_E_CLASSNOTREG shares FACILITY_ITF but isn't an MTP response code.
        assert!(matches!(hresult(0x8004_0154), Error::Other { .. }));
    }

    #[test]
    fn pc_local_offset_is_a_whole_minute_offset() {
        // Whatever zone the CI runner is in, the lookup resolves for a summer and a winter date.
        for secs in [1_560_500_000, 1_547_550_000] {
            assert!(pc_local_offset_at(secs).is_some(), "{secs}");
        }
    }
}
