//! Thin wrappers over the WPD COM interfaces. All the backend's `unsafe` lives here and in
//! [`super::props`]. One [`WpdDevice`] owns every COM pointer for a single open device; it is
//! created, used, and dropped entirely on the actor thread (the pointers are `!Send`), so nothing
//! here ever crosses a thread boundary.

use super::consts::WPD_DEVICE_OBJECT_ID;
use super::events::WpdEventSink;
use super::ids::IdMap;
use super::props::{
    is_folder_content_type, map_hresult, read_date, read_object_info, set_u32, take_pwstr, wide,
};
use crate::cancel::CancelToken;
use crate::mtp::object::NewObjectInfo;
use crate::mtp::{
    Capabilities, DeviceEvent, DeviceInfo, Error, FilesystemType, ObjectFormat, ObjectHandle,
    ObjectInfo, StorageId, StorageInfo, StorageType,
};
use futures::channel::mpsc::UnboundedSender;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use windows::core::ComObject;
use windows::core::Interface;
use windows::core::PCWSTR;
use windows::core::PWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_IDW, CM_Get_Parent, CM_Locate_DevNodeW, CM_LOCATE_DEVNODE_NORMAL, CR_SUCCESS,
};
use windows::Win32::Devices::PortableDevices::*;
use windows::Win32::Foundation::{PROPERTYKEY, S_OK};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemAlloc, CoTaskMemFree, IStream, CLSCTX_ALL, STGC_DEFAULT, STGM_READ,
    STREAM_SEEK_SET,
};
use windows::Win32::System::Variant::VT_LPWSTR;

/// One device as seen by enumeration (before opening).
pub(crate) struct DeviceEntry {
    /// The WPD PnP device id string (passed to [`WpdDevice::open`]).
    pub(crate) pnp_id: String,
}

/// Enumerate the portable devices Windows currently sees.
///
/// # Safety
/// Must run on a COM-initialized (MTA) thread.
pub(crate) unsafe fn enumerate() -> Result<Vec<DeviceEntry>, Error> {
    let manager: IPortableDeviceManager =
        CoCreateInstance(&PortableDeviceManager, None, CLSCTX_ALL).map_err(map_hresult)?;

    let mut count: u32 = 0;
    manager
        .GetDevices(std::ptr::null_mut(), &mut count)
        .map_err(map_hresult)?;
    if count == 0 {
        return Ok(Vec::new());
    }

    let mut ids: Vec<PWSTR> = vec![PWSTR::null(); count as usize];
    manager
        .GetDevices(ids.as_mut_ptr(), &mut count)
        .map_err(map_hresult)?;

    let out = ids
        .iter()
        .map(|&p| DeviceEntry {
            pnp_id: take_pwstr(p),
        })
        .collect();
    Ok(out)
}

/// An open WPD device with all its COM interface pointers. Lives only on the actor thread.
pub(crate) struct WpdDevice {
    // Field order matters for drop: interfaces release in declaration order. `device` last so the
    // session outlives the content/props/resources/event-callback derived from it.
    content: IPortableDeviceContent,
    props: IPortableDeviceProperties,
    resources: IPortableDeviceResources,
    /// The event sink we handed WPD via `Advise`. Kept alive for the lifetime of the registration
    /// (released — our reference — when this struct drops, *after* `Unadvise` in [`Drop`]). `None`
    /// until [`register_events`](Self::register_events) runs (and on a device WPD never advised).
    event_callback: Option<IPortableDeviceEventCallback>,
    /// The cookie `Advise` returned (a COM-allocated string). Null until a successful register;
    /// passed to `Unadvise` and `CoTaskMemFree`d in [`Drop`].
    event_cookie: PWSTR,
    // Held only to keep the device session alive (its content/props/resources derive from it); never
    // read directly. Released last (declared last) on drop.
    #[allow(dead_code)]
    device: IPortableDevice,
    /// Shared with the event callback so an event's WPD object-id string is interned into the *same*
    /// reverse map the worker resolves handles from. `std::sync::Mutex` (not async): held only for
    /// the fast intern/resolve, never across a COM call or `.await`.
    ids: Arc<Mutex<IdMap>>,
    /// WPD object-id strings of the device's storages, for top-level (`ROOT`) parent detection.
    storage_wpd_ids: HashSet<String>,
    device_info: DeviceInfo,
    capabilities: Capabilities,
}

impl Drop for WpdDevice {
    fn drop(&mut self) {
        // Unadvise on this (the COM apartment) thread *before* the interfaces release and COM
        // uninitializes. Only if we actually registered (non-null cookie). Then free the cookie.
        if !self.event_cookie.is_null() {
            // SAFETY: runs on the worker/COM thread while `device` is still live; `event_cookie` is
            // the COM-allocated string `Advise` returned.
            unsafe {
                let _ = self.device.Unadvise(PCWSTR(self.event_cookie.0));
                CoTaskMemFree(Some(self.event_cookie.0 as *const c_void));
            }
            self.event_cookie = PWSTR::null();
        }
    }
}

impl WpdDevice {
    /// Open a device by its WPD PnP id.
    ///
    /// # Safety
    /// Must run on a COM-initialized (MTA) thread.
    pub(crate) unsafe fn open(pnp_id: &str) -> Result<Self, Error> {
        let device: IPortableDevice =
            CoCreateInstance(&PortableDevice, None, CLSCTX_ALL).map_err(map_hresult)?;
        let client: IPortableDeviceValues =
            CoCreateInstance(&PortableDeviceValues, None, CLSCTX_ALL).map_err(map_hresult)?;
        let name = wide("mtp-rs");
        client
            .SetStringValue(&WPD_CLIENT_NAME, PCWSTR(name.as_ptr()))
            .map_err(map_hresult)?;
        let _ = set_u32(&client, &WPD_CLIENT_MAJOR_VERSION, 1);
        let _ = set_u32(&client, &WPD_CLIENT_MINOR_VERSION, 0);
        let _ = set_u32(&client, &WPD_CLIENT_REVISION, 0);

        let pnp_w = wide(pnp_id);
        device
            .Open(PCWSTR(pnp_w.as_ptr()), &client)
            .map_err(map_hresult)?;

        let content = device.Content().map_err(map_hresult)?;
        let props = content.Properties().map_err(map_hresult)?;
        let resources = content.Transfer().map_err(map_hresult)?;

        let ids = Arc::new(Mutex::new(IdMap::new()));
        let device_info = read_device_info(&props);
        let capabilities = probe_capabilities(&device);
        let storage_wpd_ids =
            collect_storage_ids(&content, &mut ids.lock().expect("idmap poisoned"));

        Ok(Self {
            content,
            props,
            resources,
            event_callback: None,
            event_cookie: PWSTR::null(),
            device,
            ids,
            storage_wpd_ids,
            device_info,
            capabilities,
        })
    }

    /// Register the WPD event callback so the device's events flow to `event_tx`.
    ///
    /// Builds the [`WpdEventSink`] (sharing the device's [`IdMap`] so event handles stay resolvable),
    /// hands WPD an interface pointer via `Advise`, and stores both the cookie (for `Unadvise` on
    /// drop) and our own reference to the sink (so it outlives the registration). Must run on the
    /// COM/worker thread. A failed `Advise` is logged and tolerated: the sink (and thus the sender)
    /// is still retained, so the event channel stays open and `next_event` simply blocks.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn register_events(&mut self, event_tx: UnboundedSender<DeviceEvent>) {
        let sink = WpdEventSink::new(event_tx, Arc::clone(&self.ids));
        let callback: IPortableDeviceEventCallback = ComObject::new(sink).into_interface();
        match self
            .device
            .Advise(0, &callback, None::<&IPortableDeviceValues>)
        {
            Ok(cookie) => self.event_cookie = cookie,
            Err(e) => eprintln!(
                "mtp-rs: WPD event registration (Advise) failed: {}",
                map_hresult(e)
            ),
        }
        self.event_callback = Some(callback);
    }

    pub(crate) fn device_info(&self) -> &DeviceInfo {
        &self.device_info
    }

    pub(crate) fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// List the storages (the device's storage/functional objects).
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn storages(&mut self) -> Result<Vec<StorageInfo>, Error> {
        let storage_ids = enum_children(&self.content, WPD_DEVICE_OBJECT_ID)?;
        let mut out = Vec::new();
        for wpd_id in storage_ids {
            if !is_storage(&self.props, &wpd_id) {
                continue;
            }
            self.storage_wpd_ids.insert(wpd_id.clone());
            let id = self.ids.lock().expect("idmap poisoned").storage(&wpd_id);
            out.push(read_storage_info(&self.props, &wpd_id, id));
        }
        Ok(out)
    }

    /// Fetch one storage's info.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn storage_info(&mut self, storage: StorageId) -> Result<StorageInfo, Error> {
        let wpd_id = self
            .ids
            .lock()
            .expect("idmap poisoned")
            .storage_id(storage)
            .ok_or(Error::NotFound)?
            .to_string();
        Ok(read_storage_info(&self.props, &wpd_id, storage))
    }

    /// List the direct children of a directory (a storage when `parent` is `None`, else a folder).
    ///
    /// Eager (reads every child's properties before returning): the COM pointers can't leave this
    /// thread, so a lazily-issuing cross-thread stream isn't possible; the `mtp::` façade wraps the
    /// returned `Vec` as a stream. The cancel token is checked between enumeration batches.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn list(
        &mut self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        cancel: Option<&CancelToken>,
    ) -> Result<Vec<ObjectInfo>, Error> {
        let (parent_wpd, child_parent) = match parent {
            None => (
                self.ids
                    .lock()
                    .expect("idmap poisoned")
                    .storage_id(storage)
                    .ok_or(Error::NotFound)?
                    .to_string(),
                ObjectHandle::ROOT,
            ),
            Some(h) => (
                self.ids
                    .lock()
                    .expect("idmap poisoned")
                    .object_id(h)
                    .ok_or(Error::StaleHandle)?
                    .to_string(),
                h,
            ),
        };

        let parent_w = wide(&parent_wpd);
        let enumerator = self
            .content
            .EnumObjects(0, PCWSTR(parent_w.as_ptr()), None)
            .map_err(map_hresult)?;

        let mut out = Vec::new();
        loop {
            if is_cancelled(cancel) {
                return Err(Error::Cancelled);
            }
            let mut batch: [PWSTR; 32] = [PWSTR::null(); 32];
            let mut fetched: u32 = 0;
            let hr = enumerator.Next(&mut batch, &mut fetched);
            for item in batch.iter().take(fetched as usize) {
                let id = take_pwstr(*item);
                // Intern under the lock, then read properties without it (no COM under the mutex).
                let handle = self.ids.lock().expect("idmap poisoned").object(&id);
                out.push(read_object_info(
                    &self.props,
                    handle,
                    &id,
                    child_parent,
                    storage,
                ));
            }
            if fetched == 0 || hr != S_OK {
                break;
            }
        }
        Ok(out)
    }

    /// Metadata for one object.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn object_info(&mut self, obj: ObjectHandle) -> Result<ObjectInfo, Error> {
        let wpd_id = self
            .ids
            .lock()
            .expect("idmap poisoned")
            .object_id(obj)
            .ok_or(Error::StaleHandle)?
            .to_string();
        let id_w = wide(&wpd_id);
        // Unlike the lenient listing reader, a single lookup must fail for a missing/deleted object,
        // so GetValues errors propagate and an unreadable name is treated as "not found".
        let v = self
            .props
            .GetValues(PCWSTR(id_w.as_ptr()), None)
            .map_err(map_hresult)?;

        let filename = v
            .GetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME)
            .map(|p| take_pwstr(p))
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                v.GetStringValue(&WPD_OBJECT_NAME)
                    .map(|p| take_pwstr(p))
                    .ok()
                    .filter(|s| !s.is_empty())
            });
        let Some(filename) = filename else {
            // Deleted objects keep a bimap entry but resolve no properties.
            return Err(Error::NotFound);
        };

        let ctype = v.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE).unwrap_or_default();
        let folder = is_folder_content_type(&ctype);
        let size = if folder {
            0
        } else {
            v.GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE)
                .unwrap_or(0)
        };
        // A parent that is a storage/functional object means a top-level object (neutral ROOT).
        let parent_id = v
            .GetStringValue(&WPD_OBJECT_PARENT_ID)
            .map(|p| take_pwstr(p))
            .unwrap_or_default();
        let parent = if parent_id.is_empty() || self.storage_wpd_ids.contains(&parent_id) {
            ObjectHandle::ROOT
        } else {
            self.ids.lock().expect("idmap poisoned").object(&parent_id)
        };
        let format = if folder {
            ObjectFormat::ASSOCIATION
        } else {
            ObjectFormat::UNDEFINED
        };

        Ok(ObjectInfo {
            handle: obj,
            // A standalone lookup doesn't know the storage; leave default (the conformance suite
            // checks filename/size/parent, not storage_id, for object_info).
            storage_id: StorageId::default(),
            parent,
            filename,
            size,
            format,
            created: read_date(&v, &WPD_OBJECT_DATE_CREATED),
            modified: read_date(&v, &WPD_OBJECT_DATE_MODIFIED),
            image_width: 0,
            image_height: 0,
            folder,
        })
    }

    /// The full byte size of an object.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn object_size(&mut self, obj: ObjectHandle) -> Result<u64, Error> {
        let wpd_id = self
            .ids
            .lock()
            .expect("idmap poisoned")
            .object_id(obj)
            .ok_or(Error::StaleHandle)?
            .to_string();
        let w = wide(&wpd_id);
        let vals = self
            .props
            .GetValues(PCWSTR(w.as_ptr()), None)
            .map_err(map_hresult)?;
        Ok(vals
            .GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE)
            .unwrap_or(0))
    }

    /// Open the default-resource read stream for an object (the whole object).
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn open_stream(&mut self, obj: ObjectHandle) -> Result<IStream, Error> {
        let wpd_id = self
            .ids
            .lock()
            .expect("idmap poisoned")
            .object_id(obj)
            .ok_or(Error::StaleHandle)?
            .to_string();
        let w = wide(&wpd_id);
        let mut optimal: u32 = 0;
        let mut stream: Option<IStream> = None;
        self.resources
            .GetStream(
                PCWSTR(w.as_ptr()),
                &WPD_RESOURCE_DEFAULT,
                STGM_READ.0,
                &mut optimal,
                &mut stream,
            )
            .map_err(map_hresult)?;
        stream.ok_or_else(|| Error::Other {
            detail: "WPD GetStream returned a null stream".into(),
        })
    }

    // ---- write path (Phase 3) ------------------------------------------------------------------

    /// Resolve the WPD parent id for a create/upload under `storage`/`parent` (`None` = storage root).
    fn parent_wpd_id(
        &self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
    ) -> Result<String, Error> {
        let ids = self.ids.lock().expect("idmap poisoned");
        match parent {
            None => ids
                .storage_id(storage)
                .map(str::to_string)
                .ok_or(Error::NotFound),
            Some(h) => ids
                .object_id(h)
                .map(str::to_string)
                .ok_or(Error::StaleHandle),
        }
    }

    /// Resolve a move/copy destination folder WPD id (`ROOT` → the storage root).
    fn dest_wpd_id(
        &self,
        new_storage: StorageId,
        new_parent: ObjectHandle,
    ) -> Result<String, Error> {
        let ids = self.ids.lock().expect("idmap poisoned");
        if new_parent == ObjectHandle::ROOT {
            ids.storage_id(new_storage)
                .map(str::to_string)
                .ok_or(Error::NotFound)
        } else {
            ids.object_id(new_parent)
                .map(str::to_string)
                .ok_or(Error::StaleHandle)
        }
    }

    /// Create a folder, returning its handle.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn create_folder(
        &mut self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        name: &str,
    ) -> Result<ObjectHandle, Error> {
        let parent_wpd = self.parent_wpd_id(storage, parent)?;
        let values: IPortableDeviceValues =
            CoCreateInstance(&PortableDeviceValues, None, CLSCTX_ALL).map_err(map_hresult)?;
        let parent_w = wide(&parent_wpd);
        let name_w = wide(name);
        values
            .SetStringValue(&WPD_OBJECT_PARENT_ID, PCWSTR(parent_w.as_ptr()))
            .map_err(map_hresult)?;
        values
            .SetStringValue(&WPD_OBJECT_NAME, PCWSTR(name_w.as_ptr()))
            .map_err(map_hresult)?;
        values
            .SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, PCWSTR(name_w.as_ptr()))
            .map_err(map_hresult)?;
        values
            .SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, &WPD_CONTENT_TYPE_FOLDER)
            .map_err(map_hresult)?;

        let mut new_id = PWSTR::null();
        self.content
            .CreateObjectWithPropertiesOnly(&values, &mut new_id)
            .map_err(map_hresult)?;
        let wpd_id = take_pwstr(new_id);
        Ok(self.ids.lock().expect("idmap poisoned").object(&wpd_id))
    }

    /// Create the object and open its data stream for a streaming upload (no `Commit` yet).
    ///
    /// Declares `info.size` up front (MTP needs the total size before the data phase) and returns the
    /// writable `IStream`. The caller writes chunks with [`stream_write`] as they arrive, then either
    /// [`commit_upload_stream`](Self::commit_upload_stream) (clean end) or drops the stream (abort).
    /// Nothing buffers the whole file: peak memory is a few in-flight chunks, not the file size.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn create_upload_stream(
        &mut self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        info: &NewObjectInfo,
    ) -> Result<IStream, Error> {
        let parent_wpd = self.parent_wpd_id(storage, parent)?;
        let values: IPortableDeviceValues =
            CoCreateInstance(&PortableDeviceValues, None, CLSCTX_ALL).map_err(map_hresult)?;
        let parent_w = wide(&parent_wpd);
        let name_w = wide(&info.filename);
        values
            .SetStringValue(&WPD_OBJECT_PARENT_ID, PCWSTR(parent_w.as_ptr()))
            .map_err(map_hresult)?;
        values
            .SetStringValue(&WPD_OBJECT_NAME, PCWSTR(name_w.as_ptr()))
            .map_err(map_hresult)?;
        values
            .SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, PCWSTR(name_w.as_ptr()))
            .map_err(map_hresult)?;
        values
            .SetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE, info.size)
            .map_err(map_hresult)?;
        values
            .SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, &WPD_CONTENT_TYPE_GENERIC_FILE)
            .map_err(map_hresult)?;

        let mut stream_opt: Option<IStream> = None;
        let mut optimal: u32 = 0;
        let mut cookie = PWSTR::null();
        self.content
            .CreateObjectWithPropertiesAndData(&values, &mut stream_opt, &mut optimal, &mut cookie)
            .map_err(map_hresult)?;
        let _ = take_pwstr(cookie); // free the optional cookie string
        stream_opt.ok_or_else(|| Error::Other {
            detail: "WPD CreateObjectWithPropertiesAndData returned a null stream".into(),
        })
    }

    /// Commit a fully-written upload stream and resolve the new object's handle.
    ///
    /// # Safety
    /// COM thread only; `stream` must be the data stream from [`create_upload_stream`] with all
    /// `info.size` bytes already written.
    pub(crate) unsafe fn commit_upload_stream(
        &mut self,
        stream: &IStream,
    ) -> Result<ObjectHandle, Error> {
        stream.Commit(STGC_DEFAULT).map_err(map_hresult)?;
        // The new object id is read from the data stream after commit.
        let data_stream: IPortableDeviceDataStream = stream.cast().map_err(map_hresult)?;
        let new_id = data_stream.GetObjectID().map_err(map_hresult)?;
        let wpd_id = take_pwstr(new_id);
        Ok(self.ids.lock().expect("idmap poisoned").object(&wpd_id))
    }

    /// Find a direct child of `parent` (a storage when `None`) whose filename matches, returning its
    /// handle. Used to probe whether an aborted upload left a partial object on the device.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn find_child_by_name(
        &mut self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        name: &str,
    ) -> Option<ObjectHandle> {
        self.list(storage, parent, None)
            .ok()?
            .into_iter()
            .find(|o| o.filename == name)
            .map(|o| o.handle)
    }

    /// Open the thumbnail-resource read stream for an object.
    ///
    /// Mirrors [`open_stream`](Self::open_stream) but requests `WPD_RESOURCE_THUMBNAIL` instead of the
    /// default resource. Objects with no thumbnail fail here (mapped to `Unsupported`/`NotFound`).
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn open_thumbnail_stream(
        &mut self,
        obj: ObjectHandle,
    ) -> Result<IStream, Error> {
        let wpd_id = self
            .ids
            .lock()
            .expect("idmap poisoned")
            .object_id(obj)
            .ok_or(Error::StaleHandle)?
            .to_string();
        let w = wide(&wpd_id);
        let mut optimal: u32 = 0;
        let mut stream: Option<IStream> = None;
        self.resources
            .GetStream(
                PCWSTR(w.as_ptr()),
                &WPD_RESOURCE_THUMBNAIL,
                STGM_READ.0,
                &mut optimal,
                &mut stream,
            )
            .map_err(map_hresult)?;
        stream.ok_or_else(|| Error::Other {
            detail: "WPD GetStream returned a null thumbnail stream".into(),
        })
    }

    /// Delete an object (recursively for folders).
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn delete(&mut self, obj: ObjectHandle) -> Result<(), Error> {
        let wpd_id = self
            .ids
            .lock()
            .expect("idmap poisoned")
            .object_id(obj)
            .ok_or(Error::StaleHandle)?
            .to_string();
        let ids = objid_collection(&wpd_id)?;
        let mut results: Option<IPortableDevicePropVariantCollection> = None;
        self.content
            .Delete(
                PORTABLE_DEVICE_DELETE_WITH_RECURSION.0 as u32,
                &ids,
                &mut results,
            )
            .map_err(map_hresult)?;
        Ok(())
    }

    /// Rename an object in place.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn rename(&mut self, obj: ObjectHandle, new_name: &str) -> Result<(), Error> {
        let wpd_id = self
            .ids
            .lock()
            .expect("idmap poisoned")
            .object_id(obj)
            .ok_or(Error::StaleHandle)?
            .to_string();
        let values: IPortableDeviceValues =
            CoCreateInstance(&PortableDeviceValues, None, CLSCTX_ALL).map_err(map_hresult)?;
        let name_w = wide(new_name);
        values
            .SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, PCWSTR(name_w.as_ptr()))
            .map_err(map_hresult)?;
        values
            .SetStringValue(&WPD_OBJECT_NAME, PCWSTR(name_w.as_ptr()))
            .map_err(map_hresult)?;
        let objid_w = wide(&wpd_id);
        self.props
            .SetValues(PCWSTR(objid_w.as_ptr()), &values)
            .map_err(map_hresult)?;
        Ok(())
    }

    /// Move an object to a new parent folder.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn move_object(
        &mut self,
        obj: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: StorageId,
    ) -> Result<(), Error> {
        let wpd_id = self
            .ids
            .lock()
            .expect("idmap poisoned")
            .object_id(obj)
            .ok_or(Error::StaleHandle)?
            .to_string();
        let dest = self.dest_wpd_id(new_storage, new_parent)?;
        let ids = objid_collection(&wpd_id)?;
        let dest_w = wide(&dest);
        let mut results: Option<IPortableDevicePropVariantCollection> = None;
        self.content
            .Move(&ids, PCWSTR(dest_w.as_ptr()), &mut results)
            .map_err(map_hresult)?;
        Ok(())
    }

    /// Copy an object into a new parent folder, returning the copy's handle.
    ///
    /// WPD's `Copy` reports results as a prop-variant collection that not all drivers populate, so we
    /// resolve the copy by listing the destination before and after, and taking an object that
    /// wasn't there before (the one with the source's name, when several appeared).
    ///
    /// Never match on the name alone: an Android phone answers a copy into a folder that already
    /// holds the name with OK and makes no copy (#59), and a name match then finds the original,
    /// so a caller renaming "the copy" renames the original. No new object is an error.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn copy_object(
        &mut self,
        obj: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: StorageId,
    ) -> Result<ObjectHandle, Error> {
        let wpd_id = self
            .ids
            .lock()
            .expect("idmap poisoned")
            .object_id(obj)
            .ok_or(Error::StaleHandle)?
            .to_string();
        let filename = self.object_info(obj)?.filename;
        let dest = self.dest_wpd_id(new_storage, new_parent)?;
        let dest_parent = (new_parent != ObjectHandle::ROOT).then_some(new_parent);
        let before: std::collections::HashSet<ObjectHandle> = self
            .list(new_storage, dest_parent, None)?
            .into_iter()
            .map(|o| o.handle)
            .collect();
        let ids = objid_collection(&wpd_id)?;
        let dest_w = wide(&dest);
        let mut results: Option<IPortableDevicePropVariantCollection> = None;
        self.content
            .Copy(&ids, PCWSTR(dest_w.as_ptr()), &mut results)
            .map_err(map_hresult)?;

        let new: Vec<_> = self
            .list(new_storage, dest_parent, None)?
            .into_iter()
            .filter(|o| o.handle != obj && !before.contains(&o.handle))
            .collect();
        // A sole newcomer under another name is a device that renamed the copy; several newcomers
        // and none with the name is something else writing to the folder, so don't guess.
        new.iter()
            .find(|o| o.filename == filename)
            .or_else(|| (new.len() == 1).then(|| &new[0]))
            .map(|o| o.handle)
            .ok_or_else(|| {
                Error::invalid_data(
                    "the device reported a copy it didn't make: no new object appeared in the \
                     destination",
                )
            })
    }
}

/// Build a one-element `IPortableDevicePropVariantCollection` holding a `VT_LPWSTR` object id, as
/// `Delete`/`Move`/`Copy` require.
unsafe fn objid_collection(wpd_id: &str) -> Result<IPortableDevicePropVariantCollection, Error> {
    let collection: IPortableDevicePropVariantCollection =
        CoCreateInstance(&PortableDevicePropVariantCollection, None, CLSCTX_ALL)
            .map_err(map_hresult)?;
    let mut pv = make_lpwstr_propvariant(wpd_id)?;
    let added = collection.Add(&pv);
    // The collection copies the value; free our copy regardless of the Add result.
    let _ = PropVariantClear(&mut pv);
    added.map_err(map_hresult)?;
    Ok(collection)
}

/// Construct a `VT_LPWSTR` `PROPVARIANT` owning a COM-allocated copy of `s` (so `PropVariantClear`
/// can free it).
unsafe fn make_lpwstr_propvariant(s: &str) -> Result<PROPVARIANT, Error> {
    let utf16: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = utf16.len() * std::mem::size_of::<u16>();
    let mem = CoTaskMemAlloc(bytes) as *mut u16;
    if mem.is_null() {
        return Err(Error::Io {
            message: "CoTaskMemAlloc failed for PROPVARIANT string".into(),
        });
    }
    std::ptr::copy_nonoverlapping(utf16.as_ptr(), mem, utf16.len());
    let mut pv = PROPVARIANT::default();
    let inner = &mut *pv.Anonymous.Anonymous;
    inner.vt = VT_LPWSTR;
    inner.Anonymous.pwszVal = PWSTR(mem);
    Ok(pv)
}

/// Bytes read-and-discarded per pass when falling back from an unsupported `Seek`.
const SEEK_DISCARD_CHUNK: usize = 256 * 1024;

/// Position a stream at `offset` (no-op at 0).
///
/// WPD resource streams are sometimes **forward-only**: `IStream::Seek` returns `E_NOTIMPL` (observed
/// on a Pixel 9 Pro XL). When the real seek fails we fall back to reading and discarding `offset`
/// bytes, which is correct for any forward read (a ranged/resumed download or `read_range`) at the
/// cost of reading the skipped prefix. Verified seekable streams take the fast path.
///
/// # Safety
/// COM thread only; `stream` must be live.
pub(crate) unsafe fn stream_seek(stream: &IStream, offset: u64) -> Result<(), Error> {
    if offset == 0 {
        return Ok(());
    }
    if stream.Seek(offset as i64, STREAM_SEEK_SET, None).is_ok() {
        return Ok(());
    }
    // Forward-only fallback: consume `offset` bytes.
    let mut discard = vec![0u8; SEEK_DISCARD_CHUNK];
    let mut remaining = offset;
    while remaining > 0 {
        let want = (remaining as usize).min(discard.len());
        let n = stream_read(stream, &mut discard[..want])?;
        if n == 0 {
            return Err(Error::invalid_data(
                "WPD stream ended before reaching the seek offset",
            ));
        }
        remaining -= n as u64;
    }
    Ok(())
}

/// Read up to `buf.len()` bytes; returns the number read (0 at EOF).
///
/// # Safety
/// COM thread only; `stream` must be live.
pub(crate) unsafe fn stream_read(stream: &IStream, buf: &mut [u8]) -> Result<usize, Error> {
    let mut read: u32 = 0;
    stream
        .Read(
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            Some(&mut read),
        )
        .ok()
        .map_err(map_hresult)?;
    Ok(read as usize)
}

/// Write `data` fully to a stream, looping over short writes.
///
/// # Safety
/// COM thread only; `stream` must be a live writable upload data stream.
pub(crate) unsafe fn stream_write(stream: &IStream, data: &[u8]) -> Result<(), Error> {
    let mut written = 0usize;
    while written < data.len() {
        let chunk = &data[written..];
        let want = u32::try_from(chunk.len()).unwrap_or(u32::MAX);
        let mut wrote: u32 = 0;
        stream
            .Write(chunk.as_ptr() as *const c_void, want, Some(&mut wrote))
            .ok()
            .map_err(map_hresult)?;
        if wrote == 0 {
            return Err(Error::Other {
                detail: "WPD stream write returned 0 bytes".into(),
            });
        }
        written += wrote as usize;
    }
    Ok(())
}

fn is_cancelled(cancel: Option<&CancelToken>) -> bool {
    cancel.is_some_and(CancelToken::is_cancelled)
}

/// The USB-descriptor serial of the physical device behind a WPD device id, via the Windows device
/// tree. Best-effort: `None` if any CfgMgr step fails.
///
/// nusb and WPD label the same device differently: nusb exposes the USB iSerial, but a WPD device
/// object is the *interface* node (`…&MI_00`) whose own `WPD_DEVICE_SERIAL_NUMBER` is a *different*
/// value. The USB serial lives on the interface's parent (the composite USB device), so we walk
/// PnP-id → devnode → parent → instance id and take its trailing segment. Used to disambiguate two
/// identical-model devices that share a VID/PID.
///
/// # Safety
/// Calls CfgMgr32 with locally-owned buffers; kept `unsafe` for symmetry with the FFI here.
pub(crate) unsafe fn wpd_device_usb_serial(wpd_pnp_id: &str) -> Option<String> {
    let instance = pnp_id_to_instance_id(wpd_pnp_id)?;
    let inst_w = wide(&instance);
    let mut devinst: u32 = 0;
    if CM_Locate_DevNodeW(
        &mut devinst,
        PCWSTR(inst_w.as_ptr()),
        CM_LOCATE_DEVNODE_NORMAL,
    ) != CR_SUCCESS
    {
        return None;
    }
    // A composite-device interface (`…&MI_xx`) hangs the serial on its parent; a single-function
    // device carries it on its own node. Walk up only for the former.
    let node = if instance.to_ascii_lowercase().contains("&mi_") {
        let mut parent: u32 = 0;
        if CM_Get_Parent(&mut parent, devinst, 0) != CR_SUCCESS {
            return None;
        }
        parent
    } else {
        devinst
    };
    let mut buf = [0u16; 512];
    if CM_Get_Device_IDW(node, &mut buf, 0) != CR_SUCCESS {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let id = String::from_utf16_lossy(&buf[..end]);
    // e.g. `USB\VID_18D1&PID_4EE2\46061FDAS000A4` → the trailing segment is the serial.
    id.rsplit('\\').next().map(str::to_string)
}

/// Convert a WPD device-interface path to a device instance id CfgMgr can locate.
/// `\\?\usb#vid_18d1&pid_4ee2&mi_00#6&…#{guid}` → `usb\vid_18d1&pid_4ee2&mi_00\6&…`
fn pnp_id_to_instance_id(pnp: &str) -> Option<String> {
    let body = pnp.strip_prefix(r"\\?\").unwrap_or(pnp);
    // Drop the trailing `#{interface-class-guid}`.
    let body = match body.rfind('#') {
        Some(i) => &body[..i],
        None => body,
    };
    (!body.is_empty()).then(|| body.replace('#', "\\"))
}

/// Enumerate the direct child object-id strings of a WPD parent id.
unsafe fn enum_children(
    content: &IPortableDeviceContent,
    parent_wpd_id: &str,
) -> Result<Vec<String>, Error> {
    let parent_w = wide(parent_wpd_id);
    let enumerator = content
        .EnumObjects(0, PCWSTR(parent_w.as_ptr()), None)
        .map_err(map_hresult)?;
    let mut out = Vec::new();
    loop {
        let mut batch: [PWSTR; 32] = [PWSTR::null(); 32];
        let mut fetched: u32 = 0;
        let hr = enumerator.Next(&mut batch, &mut fetched);
        for item in batch.iter().take(fetched as usize) {
            out.push(take_pwstr(*item));
        }
        if fetched == 0 || hr != S_OK {
            break;
        }
    }
    Ok(out)
}

/// Whether a WPD object is a storage/functional object.
unsafe fn is_storage(props: &IPortableDeviceProperties, wpd_id: &str) -> bool {
    let w = wide(wpd_id);
    let Ok(v) = props.GetValues(PCWSTR(w.as_ptr()), None) else {
        return false;
    };
    matches!(
        v.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE),
        Ok(g) if g == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT
    )
}

/// Collect the device's storage object-id strings (for ROOT-parent detection).
unsafe fn collect_storage_ids(
    content: &IPortableDeviceContent,
    ids: &mut IdMap,
) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(children) = enum_children(content, WPD_DEVICE_OBJECT_ID) {
        for wpd_id in children {
            // Intern eagerly so storage tokens exist before the first `storages()` call.
            let _ = ids.storage(&wpd_id);
            set.insert(wpd_id);
        }
    }
    set
}

/// Read the neutral device identity from the `"DEVICE"` object.
unsafe fn read_device_info(props: &IPortableDeviceProperties) -> DeviceInfo {
    let w = wide(WPD_DEVICE_OBJECT_ID);
    let Ok(v) = props.GetValues(PCWSTR(w.as_ptr()), None) else {
        return DeviceInfo::default();
    };
    let get = |key| {
        v.GetStringValue(key)
            .map(|p| take_pwstr(p))
            .unwrap_or_default()
    };
    DeviceInfo {
        manufacturer: get(&WPD_DEVICE_MANUFACTURER),
        model: get(&WPD_DEVICE_MODEL),
        serial_number: get(&WPD_DEVICE_SERIAL_NUMBER),
        device_version: get(&WPD_DEVICE_FIRMWARE_VERSION),
    }
}

/// Read one storage's neutral info from its WPD object properties.
unsafe fn read_storage_info(
    props: &IPortableDeviceProperties,
    wpd_id: &str,
    id: StorageId,
) -> StorageInfo {
    let w = wide(wpd_id);
    let vals = props.GetValues(PCWSTR(w.as_ptr()), None).ok();

    let mut info = StorageInfo {
        id,
        is_writable: true,
        storage_type: StorageType::FixedRam,
        filesystem_type: FilesystemType::Hierarchical,
        ..Default::default()
    };
    if let Some(v) = vals {
        info.description = v
            .GetStringValue(&WPD_STORAGE_DESCRIPTION)
            .map(|p| take_pwstr(p))
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                v.GetStringValue(&WPD_OBJECT_NAME)
                    .map(|p| take_pwstr(p))
                    .ok()
            })
            .unwrap_or_default();
        info.total_capacity = v
            .GetUnsignedLargeIntegerValue(&WPD_STORAGE_CAPACITY)
            .unwrap_or(0);
        info.free_space = v
            .GetUnsignedLargeIntegerValue(&WPD_STORAGE_FREE_SPACE_IN_BYTES)
            .unwrap_or(0);
        // WPD_STORAGE_ACCESS_CAPABILITY: 0 == READ_WRITE; anything else is some read-only variant.
        if let Ok(access) = v.GetUnsignedIntegerValue(&WPD_STORAGE_ACCESS_CAPABILITY) {
            info.is_writable = access == 0;
        }
    }
    info
}

/// Derive [`Capabilities`] from the device's supported WPD commands.
///
/// Reads `IPortableDeviceCapabilities::GetSupportedCommands` and maps the object-management commands
/// to the neutral flags. Falls back to permissive MTP defaults if the probe yields nothing (older
/// driver). `supports_partial_download` is always true (we provide ranged reads via seek or the
/// read-and-discard fallback); `supports_thumbnails` is true (the backend reads the
/// `WPD_RESOURCE_THUMBNAIL` resource — *whether a given object has one* is resolved at call time,
/// which is the only place WPD can answer it); `supports_events` is true (the backend registers a
/// WPD event callback at open — see [`WpdDevice::register_events`]).
///
/// # Safety
/// COM thread only.
unsafe fn probe_capabilities(device: &IPortableDevice) -> Capabilities {
    let commands: Vec<PROPERTYKEY> = (|| {
        let caps = device.Capabilities().ok()?;
        let cmds = caps.GetSupportedCommands().ok()?;
        // GetCount / GetAt write through `*const` out-pointers (a windows-rs quirk), so these need
        // no `mut` as far as the borrow checker can see.
        let count = 0u32;
        cmds.GetCount(&count).ok()?;
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let key = PROPERTYKEY::default();
            if cmds.GetAt(i, &key).is_ok() {
                out.push(key);
            }
        }
        Some(out)
    })()
    .unwrap_or_default();

    // Permissive defaults when the device doesn't enumerate commands.
    if commands.is_empty() {
        return Capabilities {
            can_upload: true,
            can_delete: true,
            can_rename: true,
            can_move: true,
            can_copy: true,
            can_create_folder: true,
            supports_partial_download: true,
            supports_thumbnails: true,
            supports_events: true,
        };
    }

    let has = |k: &PROPERTYKEY| {
        commands
            .iter()
            .any(|c| c.fmtid == k.fmtid && c.pid == k.pid)
    };
    Capabilities {
        can_upload: has(&WPD_COMMAND_OBJECT_MANAGEMENT_CREATE_OBJECT_WITH_PROPERTIES_AND_DATA),
        can_delete: has(&WPD_COMMAND_OBJECT_MANAGEMENT_DELETE_OBJECTS),
        can_rename: has(&WPD_COMMAND_OBJECT_PROPERTIES_SET),
        can_move: has(&WPD_COMMAND_OBJECT_MANAGEMENT_MOVE_OBJECTS),
        can_copy: has(&WPD_COMMAND_OBJECT_MANAGEMENT_COPY_OBJECTS),
        can_create_folder: has(&WPD_COMMAND_OBJECT_MANAGEMENT_CREATE_OBJECT_WITH_PROPERTIES_ONLY),
        supports_partial_download: true,
        supports_thumbnails: true,
        // The backend registers a WPD event callback (Advise) at open, so events are delivered.
        supports_events: true,
    }
}

#[cfg(test)]
mod tests {
    use super::pnp_id_to_instance_id;

    #[test]
    fn instance_id_strips_interface_prefix_and_class_guid() {
        // A composite-device interface path (the Pixel's MTP function).
        assert_eq!(
            pnp_id_to_instance_id(
                r"\\?\usb#vid_18d1&pid_4ee2&mi_00#6&206d8091&0&0000#{6ac27878-a6fa-4155-ba85-f98f491d4f33}"
            )
            .as_deref(),
            Some(r"usb\vid_18d1&pid_4ee2&mi_00\6&206d8091&0&0000")
        );
    }

    #[test]
    fn instance_id_handles_single_function_device() {
        assert_eq!(
            pnp_id_to_instance_id(r"\\?\usb#vid_0001&pid_0002#0123456789#{abcd}").as_deref(),
            Some(r"usb\vid_0001&pid_0002\0123456789")
        );
    }

    #[test]
    fn instance_id_rejects_empty() {
        assert_eq!(pnp_id_to_instance_id(r"\\?\#{guid}"), None);
    }
}
