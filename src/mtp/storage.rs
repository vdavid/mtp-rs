//! Storage operations.

use crate::mtp::object::NewObjectInfo;
use crate::mtp::stream::{FileDownload, Progress};
use crate::ptp::{ObjectHandle, ObjectInfo, StorageId, StorageInfo};
use crate::Error;
use bytes::Bytes;
use futures::Stream;
use std::ops::ControlFlow;
use std::sync::Arc;

use super::device::MtpDeviceInner;

/// An in-progress directory listing that yields [`ObjectInfo`] items one at a time.
///
/// Created by [`Storage::list_objects_stream()`]. After `GetObjectHandles` completes,
/// the total count is known immediately. Each call to [`next()`](Self::next) fetches
/// one `GetObjectInfo` from USB, so the consumer can report progress (e.g.,
/// "Loading files (42 of 500)...") as items arrive.
///
/// # Important
///
/// The MTP session is busy while this listing is active. You must consume
/// all items (or drop the listing) before calling other storage methods.
///
/// # Example
///
/// ```rust,no_run
/// use mtp_rs::mtp::MtpDevice;
///
/// # async fn example() -> Result<(), mtp_rs::Error> {
/// # let device = MtpDevice::open_first().await?;
/// # let storages = device.storages().await?;
/// # let storage = &storages[0];
/// let mut listing = storage.list_objects_stream(None).await?;
/// println!("Loading {} files...", listing.total());
///
/// while let Some(result) = listing.next().await {
///     let info = result?;
///     println!("[{}/{}] {}", listing.fetched(), listing.total(), info.filename);
/// }
/// # Ok(())
/// # }
/// ```
pub struct ObjectListing {
    inner: Arc<MtpDeviceInner>,
    handles: Vec<ObjectHandle>,
    /// Index of the next handle to fetch.
    cursor: usize,
    /// Parent filter: if Some, only items matching this parent are yielded.
    parent_filter: Option<ParentFilter>,
}

/// Describes how to filter objects by parent handle.
enum ParentFilter {
    /// Accept objects whose parent matches exactly.
    Exact(ObjectHandle),
    /// Android root: accept parent 0 or 0xFFFFFFFF.
    AndroidRoot,
}

impl ObjectListing {
    /// Total number of object handles returned by the device.
    ///
    /// When a parent filter is active (e.g., Fuji devices that return all objects
    /// for root), some items may be skipped, so the actual yielded count can be lower.
    #[must_use]
    pub fn total(&self) -> usize {
        self.handles.len()
    }

    /// Number of handles processed so far (including filtered-out items).
    #[must_use]
    pub fn fetched(&self) -> usize {
        self.cursor
    }

    /// Fetch the next object from the device.
    ///
    /// Returns `None` when all handles have been processed.
    /// Items that don't match the parent filter are silently skipped.
    pub async fn next(&mut self) -> Option<Result<ObjectInfo, Error>> {
        loop {
            if self.cursor >= self.handles.len() {
                return None;
            }

            let handle = self.handles[self.cursor];
            self.cursor += 1;

            let mut info = match self.inner.session.get_object_info_full(handle).await {
                Ok(info) => info,
                Err(e) => return Some(Err(e)),
            };
            info.handle = handle;

            // Apply parent filter if present
            if let Some(filter) = &self.parent_filter {
                let matches = match filter {
                    ParentFilter::Exact(expected) => info.parent == *expected,
                    ParentFilter::AndroidRoot => info.parent.0 == 0 || info.parent.0 == 0xFFFFFFFF,
                };
                if !matches {
                    continue;
                }
            }

            return Some(Ok(info));
        }
    }
}

/// A storage location on an MTP device.
///
/// `Storage` holds an `Arc<MtpDeviceInner>` so it can outlive the original
/// `MtpDevice` and be used from multiple tasks.
pub struct Storage {
    inner: Arc<MtpDeviceInner>,
    id: StorageId,
    info: StorageInfo,
}

impl Storage {
    /// Create a new Storage (internal).
    pub(crate) fn new(inner: Arc<MtpDeviceInner>, id: StorageId, info: StorageInfo) -> Self {
        Self { inner, id, info }
    }

    #[must_use]
    pub fn id(&self) -> StorageId {
        self.id
    }

    /// Storage information (cached, call refresh() to update).
    #[must_use]
    pub fn info(&self) -> &StorageInfo {
        &self.info
    }

    /// Refresh storage info from device (updates free space, etc.).
    pub async fn refresh(&mut self) -> Result<(), Error> {
        self.info = self.inner.session.get_storage_info(self.id).await?;
        Ok(())
    }

    /// List objects in a folder (None = root), returning all results at once.
    ///
    /// For progress reporting during large listings, use
    /// [`list_objects_stream()`](Self::list_objects_stream) instead.
    ///
    /// This method handles various device quirks:
    /// - Root listing tries parent=0xFFFFFFFF first (fast path for Android, Kindle, etc.)
    /// - Falls back to parent=0 only when the device rejects 0xFFFFFFFF
    /// - Samsung devices: return InvalidObjectHandle for parent=0, so we fall back to recursive
    /// - Fuji devices: return all objects for root, so we filter by parent handle
    pub async fn list_objects(
        &self,
        parent: Option<ObjectHandle>,
    ) -> Result<Vec<ObjectInfo>, Error> {
        let mut listing = self.list_objects_stream(parent).await?;
        let mut objects = Vec::with_capacity(listing.total());
        while let Some(result) = listing.next().await {
            objects.push(result?);
        }
        Ok(objects)
    }

    /// List objects in a folder as a streaming [`ObjectListing`].
    ///
    /// Returns immediately after `GetObjectHandles` completes. The total count
    /// is then known via [`ObjectListing::total()`], and each call to
    /// [`ObjectListing::next()`] fetches one object's metadata from USB.
    ///
    /// For root listings (`parent=None`), tries `parent=0xFFFFFFFF` first — this
    /// returns only root-level handles on Android, Kindle, and many other devices.
    /// Falls back to `parent=0` only when the device rejects `0xFFFFFFFF` with an
    /// error. An empty result from `0xFFFFFFFF` is treated as an empty storage,
    /// not as a reason to fall back.
    ///
    /// This enables progress reporting (e.g., "Loading 42 of 500...") during
    /// what would otherwise be a single blocking `list_objects()` call.
    ///
    /// Handles the same device quirks as [`list_objects()`](Self::list_objects).
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mtp_rs::mtp::MtpDevice;
    ///
    /// # async fn example() -> Result<(), mtp_rs::Error> {
    /// # let device = MtpDevice::open_first().await?;
    /// # let storages = device.storages().await?;
    /// # let storage = &storages[0];
    /// let mut listing = storage.list_objects_stream(None).await?;
    /// println!("Found {} items", listing.total());
    ///
    /// while let Some(result) = listing.next().await {
    ///     let info = result?;
    ///     println!("[{}/{}] {}", listing.fetched(), listing.total(), info.filename);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_objects_stream(
        &self,
        parent: Option<ObjectHandle>,
    ) -> Result<ObjectListing, Error> {
        // For root listings, try parent=0xFFFFFFFF first. Many devices (Android,
        // Kindle, others) return only root-level handles for this value, while
        // parent=0 returns every object on the storage. Fall back to parent=0
        // only when the device rejects 0xFFFFFFFF with an error.
        if parent.is_none() {
            let fast = self
                .inner
                .session
                .get_object_handles(self.id, None, Some(ObjectHandle::ALL))
                .await;

            match fast {
                Ok(handles) => {
                    return Ok(ObjectListing {
                        inner: Arc::clone(&self.inner),
                        handles,
                        cursor: 0,
                        parent_filter: Some(ParentFilter::AndroidRoot),
                    });
                }
                Err(_) => {
                    // 0xFFFFFFFF rejected; fall through to parent=0 path
                }
            }
        }

        let result = self
            .inner
            .session
            .get_object_handles(self.id, None, parent)
            .await;

        let handles = match result {
            Ok(h) => h,
            Err(Error::Protocol {
                code: crate::ptp::ResponseCode::InvalidObjectHandle,
                ..
            }) if parent.is_none() => {
                // Samsung fallback: use recursive listing and filter to root items
                return self.list_objects_stream_samsung_fallback().await;
            }
            Err(e) => return Err(e),
        };

        let parent_filter = Some(ParentFilter::Exact(parent.unwrap_or(ObjectHandle::ROOT)));

        Ok(ObjectListing {
            inner: Arc::clone(&self.inner),
            handles,
            cursor: 0,
            parent_filter,
        })
    }

    /// Samsung fallback returning a streaming [`ObjectListing`].
    async fn list_objects_stream_samsung_fallback(&self) -> Result<ObjectListing, Error> {
        let handles = self
            .inner
            .session
            .get_object_handles(self.id, None, Some(ObjectHandle::ALL))
            .await?;

        Ok(ObjectListing {
            inner: Arc::clone(&self.inner),
            handles,
            cursor: 0,
            // Root items have parent 0 or 0xFFFFFFFF (depending on device)
            parent_filter: Some(ParentFilter::AndroidRoot),
        })
    }

    /// List objects recursively.
    ///
    /// This method automatically detects Android devices and uses manual traversal
    /// for them, since Android's MTP implementation doesn't support the native
    /// `ObjectHandle::ALL` recursive listing.
    ///
    /// For non-Android devices, it tries native recursive listing first and falls
    /// back to manual traversal if the results look incomplete.
    pub async fn list_objects_recursive(
        &self,
        parent: Option<ObjectHandle>,
    ) -> Result<Vec<ObjectInfo>, Error> {
        if self.inner.is_android() {
            return self.list_objects_recursive_manual(parent).await;
        }

        let native_result = self.list_objects_recursive_native(parent).await?;

        // Heuristic: if we only got folders and no files, native listing
        // probably didn't work - fall back to manual traversal
        let has_files = native_result.iter().any(|o| o.is_file());
        if !native_result.is_empty() && !has_files {
            return self.list_objects_recursive_manual(parent).await;
        }

        Ok(native_result)
    }

    /// List objects recursively using native MTP recursive listing.
    pub async fn list_objects_recursive_native(
        &self,
        parent: Option<ObjectHandle>,
    ) -> Result<Vec<ObjectInfo>, Error> {
        let recursive_parent = if parent.is_none() {
            Some(ObjectHandle::ALL)
        } else {
            parent
        };

        let handles = self
            .inner
            .session
            .get_object_handles(self.id, None, recursive_parent)
            .await?;

        let mut objects = Vec::with_capacity(handles.len());
        for handle in handles {
            let mut info = self.inner.session.get_object_info_full(handle).await?;
            info.handle = handle;
            objects.push(info);
        }
        Ok(objects)
    }

    /// List objects recursively using manual folder traversal.
    pub async fn list_objects_recursive_manual(
        &self,
        parent: Option<ObjectHandle>,
    ) -> Result<Vec<ObjectInfo>, Error> {
        let mut result = Vec::new();
        let mut folders_to_visit = vec![parent];

        while let Some(current_parent) = folders_to_visit.pop() {
            let objects = self.list_objects(current_parent).await?;

            for obj in objects {
                if obj.is_folder() {
                    folders_to_visit.push(Some(obj.handle));
                }
                result.push(obj);
            }
        }

        Ok(result)
    }

    /// Get object metadata by handle.
    ///
    /// Files larger than 4 GB have their u64 size auto-resolved via
    /// `GetObjectPropValue(ObjectSize)`; the standard `ObjectInfo` dataset
    /// only encodes a u32 size which saturates at 4 GB - 1.
    pub async fn get_object_info(&self, handle: ObjectHandle) -> Result<ObjectInfo, Error> {
        let mut info = self.inner.session.get_object_info_full(handle).await?;
        info.handle = handle;
        Ok(info)
    }

    // =========================================================================
    // Download operations
    // =========================================================================

    /// Download a file and return all bytes.
    ///
    /// For small to medium files where you want all the data in memory.
    /// For large files or streaming to disk, use [`download_stream()`](Self::download_stream).
    pub async fn download(&self, handle: ObjectHandle) -> Result<Vec<u8>, Error> {
        self.inner.session.get_object(handle).await
    }

    /// Download a partial file (byte range).
    ///
    /// Uses the standard `GetPartialObject` operation, which has a 32-bit offset.
    /// Offsets beyond 4 GB will be silently truncated — for files larger than 4 GB,
    /// use [`download_partial_64()`](Self::download_partial_64) instead.
    pub async fn download_partial(
        &self,
        handle: ObjectHandle,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, Error> {
        self.inner
            .session
            .get_partial_object(handle, offset, size)
            .await
    }

    /// Download a partial file (byte range) with 64-bit offset support.
    ///
    /// Uses the Android/MTP extension `GetPartialObject64`, which supports offsets
    /// beyond 4 GB. Only works on devices that advertise support for it (most modern
    /// Android devices do); others return `OperationNotSupported`.
    pub async fn download_partial_64(
        &self,
        handle: ObjectHandle,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, Error> {
        self.inner
            .session
            .get_partial_object_64(handle, offset, size)
            .await
    }

    pub async fn download_thumbnail(&self, handle: ObjectHandle) -> Result<Vec<u8>, Error> {
        self.inner.session.get_thumb(handle).await
    }

    /// Download a file as a stream (true USB streaming).
    ///
    /// Unlike [`download()`](Self::download), this method yields data chunks
    /// directly from USB as they arrive, without buffering the entire file
    /// in memory. Ideal for large files or when piping data to disk.
    ///
    /// # Important
    ///
    /// The MTP session is locked while the download is active. You must either
    /// consume the entire download or call [`FileDownload::cancel()`] before
    /// dropping it.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mtp_rs::mtp::MtpDevice;
    /// use mtp_rs::ObjectHandle;
    /// use tokio::io::AsyncWriteExt;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let device = MtpDevice::open_first().await?;
    /// # let storages = device.storages().await?;
    /// # let storage = &storages[0];
    /// # let handle = ObjectHandle(1);
    /// let mut download = storage.download_stream(handle).await?;
    /// println!("Downloading {} bytes...", download.size());
    ///
    /// let mut file = tokio::fs::File::create("output.bin").await?;
    /// while let Some(chunk) = download.next_chunk().await {
    ///     let bytes = chunk?;
    ///     file.write_all(&bytes).await?;
    ///     println!("Progress: {:.1}%", download.progress() * 100.0);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn download_stream(&self, handle: ObjectHandle) -> Result<FileDownload, Error> {
        let info = self.get_object_info(handle).await?;
        let size = info.size;

        let stream = self
            .inner
            .session
            .execute_with_receive_stream(crate::ptp::OperationCode::GetObject, &[handle.0])
            .await?;

        Ok(FileDownload::new(size, stream))
    }

    // =========================================================================
    // Upload operations
    // =========================================================================

    /// Upload a file from a stream.
    ///
    /// The stream is consumed and all data is buffered before sending
    /// (MTP protocol requires knowing the total size upfront).
    ///
    /// # Arguments
    ///
    /// * `parent` - Parent folder handle (None for root)
    /// * `info` - Object metadata including filename and size
    /// * `data` - Stream of data chunks to upload
    pub async fn upload<S>(
        &self,
        parent: Option<ObjectHandle>,
        info: NewObjectInfo,
        data: S,
    ) -> Result<ObjectHandle, Error>
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin + Send,
    {
        self.upload_with_progress(parent, info, data, |_| ControlFlow::Continue(()))
            .await
    }

    /// Upload a file with progress callback.
    ///
    /// Progress is reported as data is read from the stream. Return
    /// `ControlFlow::Break(())` from the callback to cancel the upload.
    pub async fn upload_with_progress<S, F>(
        &self,
        parent: Option<ObjectHandle>,
        info: NewObjectInfo,
        data: S,
        mut on_progress: F,
    ) -> Result<ObjectHandle, Error>
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin + Send,
        F: FnMut(Progress) -> ControlFlow<()> + Send,
    {
        use futures::StreamExt;

        let total_size = info.size;
        let object_info = info.to_object_info();
        let parent_handle = parent.unwrap_or(ObjectHandle::ROOT);

        let (_, _, handle) = self
            .inner
            .session
            .send_object_info(self.id, parent_handle, &object_info)
            .await?;

        // Wrap the stream to report progress and support cancellation.
        let mut bytes_sent = 0u64;
        let progress_stream = data.map(move |chunk_result| {
            let chunk = chunk_result?;
            bytes_sent += chunk.len() as u64;
            let progress = Progress {
                bytes_transferred: bytes_sent,
                total_bytes: Some(total_size),
            };
            if let ControlFlow::Break(()) = on_progress(progress) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "cancelled",
                ));
            }
            Ok(chunk)
        });

        self.inner
            .session
            .send_object_stream(total_size, progress_stream)
            .await
            .map_err(|e| match &e {
                Error::Io(io_err) if io_err.kind() == std::io::ErrorKind::Interrupted => {
                    Error::Cancelled
                }
                _ => e,
            })?;

        Ok(handle)
    }

    // =========================================================================
    // Folder and object management
    // =========================================================================

    pub async fn create_folder(
        &self,
        parent: Option<ObjectHandle>,
        name: &str,
    ) -> Result<ObjectHandle, Error> {
        let info = NewObjectInfo::folder(name);
        let object_info = info.to_object_info();
        let parent_handle = parent.unwrap_or(ObjectHandle::ROOT);

        let (_, _, handle) = self
            .inner
            .session
            .send_object_info(self.id, parent_handle, &object_info)
            .await?;

        Ok(handle)
    }

    pub async fn delete(&self, handle: ObjectHandle) -> Result<(), Error> {
        self.inner.session.delete_object(handle).await
    }

    /// Move an object to a different folder.
    pub async fn move_object(
        &self,
        handle: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: Option<StorageId>,
    ) -> Result<(), Error> {
        let storage = new_storage.unwrap_or(self.id);
        self.inner
            .session
            .move_object(handle, storage, new_parent)
            .await
    }

    pub async fn copy_object(
        &self,
        handle: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: Option<StorageId>,
    ) -> Result<ObjectHandle, Error> {
        let storage = new_storage.unwrap_or(self.id);
        self.inner
            .session
            .copy_object(handle, storage, new_parent)
            .await
    }

    /// Rename an object (file or folder).
    ///
    /// Not all devices support renaming. Use `MtpDevice::supports_rename()`
    /// to check if this operation is available.
    pub async fn rename(&self, handle: ObjectHandle, new_name: &str) -> Result<(), Error> {
        self.inner.session.rename_object(handle, new_name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::{
        pack_u16, pack_u32, pack_u32_array, ContainerType, DateTime, DeviceInfo, ObjectFormatCode,
        OperationCode, PtpSession, ResponseCode, StorageInfo,
    };
    use crate::transport::mock::MockTransport;

    // -- Test helpers (same protocol-level helpers as session tests) -----------

    fn mock_transport() -> (Arc<dyn crate::transport::Transport>, Arc<MockTransport>) {
        let mock = Arc::new(MockTransport::new());
        let transport: Arc<dyn crate::transport::Transport> = Arc::clone(&mock) as _;
        (transport, mock)
    }

    fn ok_response(tx_id: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(&pack_u32(12));
        buf.extend_from_slice(&pack_u16(ContainerType::Response.to_code()));
        buf.extend_from_slice(&pack_u16(ResponseCode::Ok.into()));
        buf.extend_from_slice(&pack_u32(tx_id));
        buf
    }

    fn error_response(tx_id: u32, code: ResponseCode) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(&pack_u32(12));
        buf.extend_from_slice(&pack_u16(ContainerType::Response.to_code()));
        buf.extend_from_slice(&pack_u16(code.into()));
        buf.extend_from_slice(&pack_u32(tx_id));
        buf
    }

    fn data_container(tx_id: u32, code: OperationCode, payload: &[u8]) -> Vec<u8> {
        let len = 12 + payload.len();
        let mut buf = Vec::with_capacity(len);
        buf.extend_from_slice(&pack_u32(len as u32));
        buf.extend_from_slice(&pack_u16(ContainerType::Data.to_code()));
        buf.extend_from_slice(&pack_u16(code.into()));
        buf.extend_from_slice(&pack_u32(tx_id));
        buf.extend_from_slice(payload);
        buf
    }

    // -- Storage-level helpers ------------------------------------------------

    /// Build a Storage backed by a mock transport for testing.
    ///
    /// Queues the OpenSession response automatically. The caller must queue
    /// further responses before calling list methods.
    async fn mock_storage(
        transport: Arc<dyn crate::transport::Transport>,
        vendor_extension_desc: &str,
    ) -> Storage {
        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let inner = Arc::new(MtpDeviceInner {
            session,
            device_info: DeviceInfo {
                vendor_extension_desc: vendor_extension_desc.to_string(),
                ..DeviceInfo::default()
            },
        });
        Storage::new(inner, StorageId(1), StorageInfo::default())
    }

    /// Build a minimal ObjectInfo binary payload with a given filename and parent.
    fn object_info_bytes(filename: &str, parent: u32) -> Vec<u8> {
        let info = ObjectInfo {
            storage_id: StorageId(1),
            format: ObjectFormatCode::Jpeg,
            parent: ObjectHandle(parent),
            filename: filename.to_string(),
            created: Some(DateTime {
                year: 2024,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
            }),
            ..ObjectInfo::default()
        };
        info.to_bytes().unwrap()
    }

    /// Queue GetObjectHandles response (data + ok) for a given transaction ID.
    fn queue_handles(mock: &MockTransport, tx_id: u32, handles: &[u32]) {
        let data = pack_u32_array(handles);
        mock.queue_response(data_container(
            tx_id,
            OperationCode::GetObjectHandles,
            &data,
        ));
        mock.queue_response(ok_response(tx_id));
    }

    /// Queue GetObjectInfo response (data + ok) for a given transaction ID.
    fn queue_object_info(mock: &MockTransport, tx_id: u32, filename: &str, parent: u32) {
        let data = object_info_bytes(filename, parent);
        mock.queue_response(data_container(tx_id, OperationCode::GetObjectInfo, &data));
        mock.queue_response(ok_response(tx_id));
    }

    // -- Tests ----------------------------------------------------------------

    #[tokio::test]
    async fn stream_returns_objects_with_correct_counters() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        queue_handles(&mock, 1, &[10, 20, 30]);
        queue_object_info(&mock, 2, "photo.jpg", 0);
        queue_object_info(&mock, 3, "video.mp4", 0);
        queue_object_info(&mock, 4, "notes.txt", 0);

        let storage = mock_storage(transport, "").await;
        let mut listing = storage.list_objects_stream(None).await.unwrap();

        assert_eq!(listing.total(), 3);
        assert_eq!(listing.fetched(), 0);

        let first = listing.next().await.unwrap().unwrap();
        assert_eq!(first.filename, "photo.jpg");
        assert_eq!(first.handle, ObjectHandle(10));
        assert_eq!(listing.fetched(), 1);

        let second = listing.next().await.unwrap().unwrap();
        assert_eq!(second.filename, "video.mp4");
        assert_eq!(second.handle, ObjectHandle(20));
        assert_eq!(listing.fetched(), 2);

        let third = listing.next().await.unwrap().unwrap();
        assert_eq!(third.filename, "notes.txt");
        assert_eq!(third.handle, ObjectHandle(30));
        assert_eq!(listing.fetched(), 3);

        assert!(listing.next().await.is_none());
    }

    #[tokio::test]
    async fn stream_empty_directory() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        queue_handles(&mock, 1, &[]);

        let storage = mock_storage(transport, "").await;
        let mut listing = storage.list_objects_stream(None).await.unwrap();

        assert_eq!(listing.total(), 0);
        assert!(listing.next().await.is_none());
    }

    #[tokio::test]
    async fn stream_filters_by_parent() {
        // Root fast path (0xFFFFFFFF) uses AndroidRoot filter, which rejects
        // objects whose parent is neither 0 nor 0xFFFFFFFF.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        queue_handles(&mock, 1, &[10, 20, 30]);
        queue_object_info(&mock, 2, "root_file.jpg", 0); // parent=ROOT, included
        queue_object_info(&mock, 3, "nested.jpg", 99); // parent=99, filtered out
        queue_object_info(&mock, 4, "another_root.txt", 0); // parent=ROOT, included

        let storage = mock_storage(transport, "").await;
        let mut listing = storage.list_objects_stream(None).await.unwrap();

        assert_eq!(listing.total(), 3); // Total handles from device

        let first = listing.next().await.unwrap().unwrap();
        assert_eq!(first.filename, "root_file.jpg");
        assert_eq!(listing.fetched(), 1);

        let second = listing.next().await.unwrap().unwrap();
        assert_eq!(second.filename, "another_root.txt");
        assert_eq!(listing.fetched(), 3); // Processed all 3 (including filtered one)

        assert!(listing.next().await.is_none());
    }

    #[tokio::test]
    async fn stream_root_accepts_both_parent_values() {
        // Root fast path uses AndroidRoot filter: accepts parent 0 or 0xFFFFFFFF
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        queue_handles(&mock, 1, &[10, 20, 30]);
        queue_object_info(&mock, 2, "dcim", 0); // parent=0, root
        queue_object_info(&mock, 3, "download", 0xFFFFFFFF); // parent=ALL, also root
        queue_object_info(&mock, 4, "nested", 42); // not root

        let storage = mock_storage(transport, "").await;
        let mut listing = storage.list_objects_stream(None).await.unwrap();

        let first = listing.next().await.unwrap().unwrap();
        assert_eq!(first.filename, "dcim");

        let second = listing.next().await.unwrap().unwrap();
        assert_eq!(second.filename, "download");

        assert!(listing.next().await.is_none()); // "nested" filtered out
    }

    #[tokio::test]
    async fn stream_subfolder_listing() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        let parent_handle = 42u32;
        queue_handles(&mock, 1, &[100, 101]);
        queue_object_info(&mock, 2, "IMG_001.jpg", parent_handle);
        queue_object_info(&mock, 3, "IMG_002.jpg", parent_handle);

        let storage = mock_storage(transport, "").await;
        let mut listing = storage
            .list_objects_stream(Some(ObjectHandle(parent_handle)))
            .await
            .unwrap();

        assert_eq!(listing.total(), 2);
        let first = listing.next().await.unwrap().unwrap();
        assert_eq!(first.filename, "IMG_001.jpg");
        let second = listing.next().await.unwrap().unwrap();
        assert_eq!(second.filename, "IMG_002.jpg");
        assert!(listing.next().await.is_none());
    }

    #[tokio::test]
    async fn stream_propagates_mid_listing_error() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        queue_handles(&mock, 1, &[10, 20]);
        queue_object_info(&mock, 2, "good.jpg", 0);
        // Handle 20: device returns error instead of object info
        mock.queue_response(error_response(3, ResponseCode::InvalidObjectHandle));

        let storage = mock_storage(transport, "").await;
        let mut listing = storage.list_objects_stream(None).await.unwrap();

        let first = listing.next().await.unwrap().unwrap();
        assert_eq!(first.filename, "good.jpg");

        let second = listing.next().await.unwrap();
        assert!(second.is_err());
    }

    #[tokio::test]
    async fn list_objects_matches_stream_collect() {
        // Verify list_objects() returns identical results to collecting the stream
        let (transport1, mock1) = mock_transport();
        let (transport2, mock2) = mock_transport();

        for mock in [&mock1, &mock2] {
            mock.queue_response(ok_response(0)); // OpenSession
            queue_handles(mock, 1, &[10, 20]);
            queue_object_info(mock, 2, "a.jpg", 0);
            queue_object_info(mock, 3, "b.txt", 0);
        }

        let storage1 = mock_storage(transport1, "").await;
        let storage2 = mock_storage(transport2, "").await;

        let all_at_once = storage1.list_objects(None).await.unwrap();

        let mut listing = storage2.list_objects_stream(None).await.unwrap();
        let mut streamed = Vec::new();
        while let Some(result) = listing.next().await {
            streamed.push(result.unwrap());
        }

        assert_eq!(all_at_once.len(), streamed.len());
        for (a, b) in all_at_once.iter().zip(streamed.iter()) {
            assert_eq!(a.filename, b.filename);
            assert_eq!(a.handle, b.handle);
        }
    }

    // -- Root listing fast-path / fallback tests --------------------------------

    #[tokio::test]
    async fn stream_root_falls_back_on_error() {
        // When 0xFFFFFFFF is rejected, falls through to parent=0 with Exact(ROOT)
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // Fast path (0xFFFFFFFF): device rejects with InvalidObjectHandle
        mock.queue_response(error_response(1, ResponseCode::InvalidObjectHandle));

        // Fallback path (parent=0): returns root objects
        queue_handles(&mock, 2, &[10, 20]);
        queue_object_info(&mock, 3, "root.jpg", 0);
        queue_object_info(&mock, 4, "nested.jpg", 99); // filtered by Exact(ROOT)

        let storage = mock_storage(transport, "").await;
        let mut listing = storage.list_objects_stream(None).await.unwrap();

        assert_eq!(listing.total(), 2);

        let first = listing.next().await.unwrap().unwrap();
        assert_eq!(first.filename, "root.jpg");

        // nested.jpg has parent=99, filtered out by Exact(ROOT)
        assert!(listing.next().await.is_none());
    }

    #[tokio::test]
    async fn stream_root_empty_is_not_fallback() {
        // Empty Ok([]) from 0xFFFFFFFF is a valid empty storage, not a fallback trigger
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        queue_handles(&mock, 1, &[]); // fast path returns empty

        let storage = mock_storage(transport, "").await;
        let mut listing = storage.list_objects_stream(None).await.unwrap();

        assert_eq!(listing.total(), 0);
        assert!(listing.next().await.is_none());
    }

    #[tokio::test]
    async fn stream_kindle_root_uses_fast_path() {
        // Non-Android device (Kindle) benefits from 0xFFFFFFFF fast path
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        queue_handles(&mock, 1, &[100, 101, 102]);
        queue_object_info(&mock, 2, "documents", 0);
        queue_object_info(&mock, 3, "system", 0);
        queue_object_info(&mock, 4, "fonts", 0);

        let storage = mock_storage(transport, "microsoft.com/WMDRMPD:10.1").await;
        let mut listing = storage.list_objects_stream(None).await.unwrap();

        assert_eq!(listing.total(), 3);

        let first = listing.next().await.unwrap().unwrap();
        assert_eq!(first.filename, "documents");
        let second = listing.next().await.unwrap().unwrap();
        assert_eq!(second.filename, "system");
        let third = listing.next().await.unwrap().unwrap();
        assert_eq!(third.filename, "fonts");

        assert!(listing.next().await.is_none());
    }

    #[tokio::test]
    async fn stream_subfolder_skips_fast_path() {
        // Subfolder listing should not attempt 0xFFFFFFFF; only one get_object_handles call
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        let parent_handle = 50u32;
        queue_handles(&mock, 1, &[200, 201]);
        queue_object_info(&mock, 2, "child_a.txt", parent_handle);
        queue_object_info(&mock, 3, "child_b.txt", parent_handle);

        let storage = mock_storage(transport, "").await;
        let mut listing = storage
            .list_objects_stream(Some(ObjectHandle(parent_handle)))
            .await
            .unwrap();

        assert_eq!(listing.total(), 2);
        let first = listing.next().await.unwrap().unwrap();
        assert_eq!(first.filename, "child_a.txt");
        let second = listing.next().await.unwrap().unwrap();
        assert_eq!(second.filename, "child_b.txt");
        assert!(listing.next().await.is_none());
    }

    // -- Full-size (>4 GB) resolution via GetObjectPropValue ------------------

    /// Build an ObjectInfo payload with a specific `size`. Sizes > u32::MAX are
    /// serialized as u32::MAX by `ObjectInfo::to_bytes`, matching real-device behavior.
    fn object_info_bytes_with_size(filename: &str, parent: u32, size: u64) -> Vec<u8> {
        let info = ObjectInfo {
            storage_id: StorageId(1),
            format: ObjectFormatCode::Jpeg,
            parent: ObjectHandle(parent),
            filename: filename.to_string(),
            size,
            ..ObjectInfo::default()
        };
        info.to_bytes().unwrap()
    }

    fn queue_object_info_with_size(
        mock: &MockTransport,
        tx_id: u32,
        filename: &str,
        parent: u32,
        size: u64,
    ) {
        let data = object_info_bytes_with_size(filename, parent, size);
        mock.queue_response(data_container(tx_id, OperationCode::GetObjectInfo, &data));
        mock.queue_response(ok_response(tx_id));
    }

    fn queue_object_size_prop(mock: &MockTransport, tx_id: u32, size: u64) {
        let payload = crate::ptp::pack_u64(size);
        mock.queue_response(data_container(
            tx_id,
            OperationCode::GetObjectPropValue,
            &payload,
        ));
        mock.queue_response(ok_response(tx_id));
    }

    #[tokio::test]
    async fn get_object_info_resolves_saturated_size() {
        // Simulate a 5 GB file: ObjectInfo reports u32::MAX, GetObjectPropValue returns real u64.
        const REAL_SIZE: u64 = 5 * 1024 * 1024 * 1024;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        queue_object_info_with_size(&mock, 1, "big.mkv", 0, REAL_SIZE);
        queue_object_size_prop(&mock, 2, REAL_SIZE);

        let storage = mock_storage(transport, "").await;
        let info = storage.get_object_info(ObjectHandle(42)).await.unwrap();

        assert_eq!(info.filename, "big.mkv");
        assert_eq!(info.size, REAL_SIZE, "size should be resolved to full u64");
    }

    #[tokio::test]
    async fn get_object_info_skips_lookup_when_size_fits_u32() {
        // Under u32::MAX: GetObjectPropValue must NOT be called. If it were, the test
        // would hang or fail because we only queue one response.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        queue_object_info_with_size(&mock, 1, "small.jpg", 0, 1_000_000);

        let storage = mock_storage(transport, "").await;
        let info = storage.get_object_info(ObjectHandle(42)).await.unwrap();

        assert_eq!(info.size, 1_000_000);
    }

    #[tokio::test]
    async fn get_object_info_falls_back_when_prop_lookup_fails() {
        // Device reports saturated size but doesn't support GetObjectPropValue.
        // Caller should receive the saturated value rather than an error.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        queue_object_info_with_size(&mock, 1, "big.mkv", 0, 8 * 1024 * 1024 * 1024);
        mock.queue_response(error_response(2, ResponseCode::OperationNotSupported));

        let storage = mock_storage(transport, "").await;
        let info = storage.get_object_info(ObjectHandle(42)).await.unwrap();

        assert_eq!(
            info.size,
            u64::from(u32::MAX),
            "should keep saturated u32::MAX when prop lookup fails"
        );
    }

    #[tokio::test]
    async fn get_object_info_exactly_u32_max_triggers_lookup() {
        // A file whose real size happens to equal u32::MAX is ambiguous: we can't
        // distinguish it from a saturated >4GB file. The lookup runs and returns the
        // true size, which in this case happens to match. Verify we handle it correctly.
        const REAL_SIZE: u64 = u32::MAX as u64;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        queue_object_info_with_size(&mock, 1, "edge.bin", 0, REAL_SIZE);
        queue_object_size_prop(&mock, 2, REAL_SIZE);

        let storage = mock_storage(transport, "").await;
        let info = storage.get_object_info(ObjectHandle(42)).await.unwrap();

        assert_eq!(info.size, REAL_SIZE);
    }

    #[tokio::test]
    async fn list_objects_stream_resolves_saturated_size() {
        // Verify the fix also applies to the streaming listing path.
        const REAL_SIZE: u64 = 6 * 1024 * 1024 * 1024;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        queue_handles(&mock, 1, &[10, 20]);
        queue_object_info_with_size(&mock, 2, "small.jpg", 0, 500_000);
        queue_object_info_with_size(&mock, 3, "huge.mkv", 0, REAL_SIZE);
        queue_object_size_prop(&mock, 4, REAL_SIZE);

        let storage = mock_storage(transport, "").await;
        let objects = storage.list_objects(None).await.unwrap();

        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].size, 500_000);
        assert_eq!(objects[1].size, REAL_SIZE);
    }
}
