//! Storage operations (a thin façade over the active backend).

use crate::cancel::{bail_if_cancelled, CancelToken};
use crate::mtp::backend::{
    BackendListing, BackendListingError, ByteRange, ListingErrorDisposition, MtpBackend, ProgressFn,
};
use crate::mtp::object::NewObjectInfo;
use crate::mtp::stream::{FileDownload, Progress, WindowedDownload, DEFAULT_DOWNLOAD_WINDOW};
use crate::mtp::{Error, ObjectHandle, ObjectInfo, StorageId, StorageInfo, UploadError};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use std::ops::ControlFlow;
use std::sync::Arc;

/// An in-progress directory listing that yields [`ObjectInfo`] items one at a time.
///
/// Created by [`Storage::list_objects_stream()`]. After the device returns the handle list, the
/// total count is known immediately ([`total()`](Self::total)). Each call to [`next()`](Self::next)
/// fetches one object's metadata, so the consumer can report progress (e.g.,
/// "Loading files (42 of 500)...") as items arrive.
///
/// # Important
///
/// The device is busy while this listing is active. You must consume all items (or drop the
/// listing) before calling other storage methods.
///
/// # Example
///
/// ```rust,no_run
/// use mtp_rs::mtp::{ListingItem, MtpDevice};
///
/// # async fn example() -> Result<(), mtp_rs::Error> {
/// # let device = MtpDevice::open_first().await?;
/// # let storages = device.storages().await?;
/// # let storage = &storages[0];
/// let mut listing = storage.list_objects_stream(None).await?;
/// println!("Loading {} files...", listing.total());
///
/// while let Some(item) = listing.next().await {
///     match item? {
///         ListingItem::Object(info) => {
///             println!("[{}/{}] {}", listing.fetched(), listing.total(), info.filename);
///         }
///         ListingItem::Skipped(skipped) => {
///             eprintln!("could not read handle {}: {}", skipped.handle.0, skipped.error);
///         }
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub struct ObjectListing {
    inner: BackendListing,
    /// Items the backend has already yielded (post-filter).
    fetched: usize,
}

impl ObjectListing {
    fn new(inner: BackendListing) -> Self {
        Self { inner, fetched: 0 }
    }

    /// Total number of object handles returned by the device.
    ///
    /// When a parent filter is active (e.g. devices that return all objects for root), some items
    /// may be skipped, so the actual yielded count can be lower.
    #[must_use]
    pub fn total(&self) -> usize {
        self.inner.total
    }

    /// Number of items yielded so far.
    #[must_use]
    pub fn fetched(&self) -> usize {
        self.fetched
    }

    async fn next_classified(&mut self) -> Option<Result<ObjectInfo, BackendListingError>> {
        match self.inner.items.next().await {
            Some(Ok(info)) => {
                self.fetched += 1;
                Some(Ok(info))
            }
            other => other,
        }
    }

    /// Fetch the next item from the device.
    ///
    /// Returns `None` when the listing is exhausted. Items that don't match the parent filter are
    /// skipped by the backend and never surface here.
    ///
    /// The `Ok` side has two shapes, and the distinction is the whole point: a
    /// [`ListingItem::Object`] is an object whose metadata was read, and a
    /// [`ListingItem::Skipped`] is one handle the device refused in a way that leaves the rest of
    /// the listing usable (see [`Storage::collect_objects`] for what qualifies). An `Err` means the
    /// listing itself is over: transport trouble, a broken session, cancellation, a malformed
    /// response.
    ///
    /// So `Err` is "stop", `Ok(Skipped)` is "this one is unreadable, keep going", and you can't
    /// confuse them by accident. Consumers that don't care can filter:
    ///
    /// ```no_run
    /// # use mtp_rs::{ListingItem, Storage};
    /// # async fn demo(storage: &Storage) -> Result<(), mtp_rs::Error> {
    /// let mut listing = storage.list_objects_stream(None).await?;
    /// while let Some(item) = listing.next().await {
    ///     if let ListingItem::Object(info) = item? {
    ///         println!("{}", info.filename);
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// If a [`CancelToken`] was passed via [`Storage::list_objects_stream_with_cancel`] and it's
    /// been cancelled, this returns `Some(Err(Error::Cancelled))` at the next per-handle boundary.
    pub async fn next(&mut self) -> Option<Result<ListingItem, Error>> {
        match self.next_classified().await? {
            Ok(info) => Some(Ok(ListingItem::Object(info))),
            Err(error) if error.disposition == ListingErrorDisposition::SkipObject => {
                Some(Ok(ListingItem::Skipped(SkippedObject {
                    handle: error.handle,
                    error: error.source,
                })))
            }
            Err(error) => Some(Err(error.source)),
        }
    }
}

/// One item from an [`ObjectListing`].
///
/// The streaming and collecting APIs sit over the same stream, so they agree on what a per-object
/// failure means: this enum is the streaming half of the [`ObjectCollection`] split.
#[derive(Debug)]
pub enum ListingItem {
    /// An object whose metadata the device returned.
    Object(ObjectInfo),
    /// A handle the device refused, safely enough that the rest of the listing continues.
    Skipped(SkippedObject),
}

impl ListingItem {
    /// The object, or `None` if this item was skipped.
    #[must_use]
    pub fn object(self) -> Option<ObjectInfo> {
        match self {
            ListingItem::Object(info) => Some(info),
            ListingItem::Skipped(_) => None,
        }
    }
}

/// A per-handle metadata failure that was safe to skip while reading the
/// other objects in the same directory.
#[derive(Debug)]
pub struct SkippedObject {
    /// The object handle whose metadata request failed.
    pub handle: ObjectHandle,
    /// The backend-neutral error reported for that handle.
    pub error: Error,
}

/// A completed tolerant directory read: what was readable, and what wasn't.
#[derive(Debug)]
pub struct ObjectCollection {
    /// Every object whose metadata was read successfully.
    pub objects: Vec<ObjectInfo>,
    /// Per-handle failures that met the library's narrow safe-to-skip policy.
    pub skipped: Vec<SkippedObject>,
}

/// A storage location on an MTP device.
///
/// `Storage` holds a shared reference to the active backend so it can outlive the original
/// `MtpDevice` and be used from multiple tasks.
pub struct Storage {
    backend: Arc<dyn MtpBackend>,
    id: StorageId,
    info: StorageInfo,
}

impl Storage {
    /// Create a new Storage (internal).
    pub(crate) fn new(backend: Arc<dyn MtpBackend>, id: StorageId, info: StorageInfo) -> Self {
        Self { backend, id, info }
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
        self.info = self.backend.storage_info(self.id).await?;
        Ok(())
    }

    // =========================================================================
    // Listing
    // =========================================================================

    /// List objects in a folder (None = root), returning all results at once.
    ///
    /// For progress reporting during large listings, use
    /// [`list_objects_stream()`](Self::list_objects_stream) instead.
    ///
    /// The backend handles device quirks (root-listing fast path and Android/Samsung/Fuji
    /// fallbacks).
    ///
    /// A narrowly tolerated per-object metadata rejection does not hide valid
    /// siblings. Use [`collect_objects`](Self::collect_objects) to
    /// retain its handle and diagnostic, or the streaming API to observe every
    /// item error directly. All errors that can compromise enumeration or
    /// session integrity remain fatal.
    pub async fn list_objects(
        &self,
        parent: Option<ObjectHandle>,
    ) -> Result<Vec<ObjectInfo>, Error> {
        self.list_objects_with_cancel(parent, None).await
    }

    /// Like [`list_objects`](Self::list_objects), but takes a cooperative cancellation token.
    ///
    /// When `cancel` is `Some(&token)` and the token has been cancelled, the call bails between
    /// per-handle fetches with `Err(Error::Cancelled)`. Useful for large folders (1k+ entries on
    /// Android), where the per-handle loop dominates wall-clock time.
    pub async fn list_objects_with_cancel(
        &self,
        parent: Option<ObjectHandle>,
        cancel: Option<&CancelToken>,
    ) -> Result<Vec<ObjectInfo>, Error> {
        Ok(self
            .collect_objects_with_cancel(parent, cancel)
            .await?
            .objects)
    }

    /// Read a folder, keeping both the objects and a record of the handles that
    /// couldn't be read.
    ///
    /// [`list_objects`](Self::list_objects) is the same read with the record
    /// thrown away. Use this one when you need to tell "the folder has 49 files"
    /// from "the folder has 49 files and a 50th we couldn't see", which is the
    /// difference between a correct file listing and a silent omission.
    ///
    /// # What counts as skippable
    ///
    /// A per-handle failure may be skipped only when all three hold:
    ///
    /// 1. The handle list is already in hand, so the folder's membership isn't in
    ///    doubt, only one entry's metadata.
    /// 2. The failing operation is read-only, so nothing on the device changed.
    /// 3. The device answered with a protocol response code, which closes that
    ///    transaction cleanly and leaves the session usable for the next handle.
    ///
    /// Today exactly one case qualifies: a `GeneralError` response to
    /// `GetObjectInfo` (Sphaira on the Nintendo Switch does this for one handle
    /// out of 50). The rule is written down rather than the code, so adding a
    /// second response code is a one-line change once a real device justifies it.
    /// Nothing gets added speculatively.
    ///
    /// Everything else stays fatal: transport and session failures, malformed
    /// responses, cancellation, stale handles, and any failure to enumerate the
    /// handles in the first place. And if *every* handle was skipped, that's a
    /// device that answered nothing, so this reports the failure rather than an
    /// empty folder.
    pub async fn collect_objects(
        &self,
        parent: Option<ObjectHandle>,
    ) -> Result<ObjectCollection, Error> {
        self.collect_objects_with_cancel(parent, None).await
    }

    /// Like [`collect_objects`](Self::collect_objects), but with a cooperative
    /// cancellation token.
    pub async fn collect_objects_with_cancel(
        &self,
        parent: Option<ObjectHandle>,
        cancel: Option<&CancelToken>,
    ) -> Result<ObjectCollection, Error> {
        let mut listing = self.list_objects_stream_with_cancel(parent, cancel).await?;
        let mut objects = Vec::with_capacity(listing.total());
        let mut skipped = Vec::new();
        while let Some(result) = listing.next_classified().await {
            match result {
                Ok(object) => objects.push(object),
                Err(error) if error.disposition == ListingErrorDisposition::SkipObject => {
                    diag_debug!(
                        "list_objects: skipping handle {} on storage {} after a completed per-object metadata error: {}",
                        error.handle.0,
                        self.id.0,
                        error.source
                    );
                    skipped.push(SkippedObject {
                        handle: error.handle,
                        error: error.source,
                    });
                }
                Err(error) => return Err(error.source),
            }
        }

        // Tolerating one bad object is the point. Reporting a device that answered
        // NOTHING as an empty folder is not: `Ok(vec![])` renders as "empty folder"
        // in a file manager and reads as "everything was deleted" to anything
        // syncing, which turns a read failure into data loss. The device gave us
        // handles and then failed every single lookup, so we learned nothing about
        // a folder we know has contents. That's a failure, not a result.
        if objects.is_empty() && !skipped.is_empty() {
            let first = skipped.swap_remove(0);
            diag_debug!(
                "list_objects: every one of {} handles on storage {} failed its metadata lookup; \
                 reporting the failure rather than an empty folder",
                skipped.len() + 1,
                self.id.0
            );
            return Err(first.error);
        }

        Ok(ObjectCollection { objects, skipped })
    }

    /// List objects in a folder as a streaming [`ObjectListing`].
    ///
    /// Returns immediately after the device returns the handle list. The total count is then known
    /// via [`ObjectListing::total()`], and each call to [`ObjectListing::next()`] fetches one
    /// object's metadata.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mtp_rs::mtp::{ListingItem, MtpDevice};
    ///
    /// # async fn example() -> Result<(), mtp_rs::Error> {
    /// # let device = MtpDevice::open_first().await?;
    /// # let storages = device.storages().await?;
    /// # let storage = &storages[0];
    /// let mut listing = storage.list_objects_stream(None).await?;
    /// println!("Found {} items", listing.total());
    ///
    /// while let Some(item) = listing.next().await {
    ///     if let ListingItem::Object(info) = item? {
    ///         println!("[{}/{}] {}", listing.fetched(), listing.total(), info.filename);
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_objects_stream(
        &self,
        parent: Option<ObjectHandle>,
    ) -> Result<ObjectListing, Error> {
        self.list_objects_stream_with_cancel(parent, None).await
    }

    /// Like [`list_objects_stream`](Self::list_objects_stream), but the returned [`ObjectListing`]
    /// carries an optional [`CancelToken`]. Every call to [`ObjectListing::next`] checks the token
    /// before issuing the next metadata roundtrip, so a flipped token bails within one roundtrip's
    /// worth of latency instead of running to completion.
    pub async fn list_objects_stream_with_cancel(
        &self,
        parent: Option<ObjectHandle>,
        cancel: Option<&CancelToken>,
    ) -> Result<ObjectListing, Error> {
        let listing = self.backend.list(self.id, parent, cancel).await?;
        Ok(ObjectListing::new(listing))
    }

    /// List objects recursively.
    ///
    /// Walks the folder tree manually via [`list_objects`](Self::list_objects), which already
    /// applies the backend's root/quirk handling. Works the same across all devices, including
    /// Android (whose native `GetObjectHandles` recursion is broken).
    pub async fn list_objects_recursive(
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
    /// Files larger than 4 GB have their u64 size auto-resolved by the backend.
    pub async fn get_object_info(&self, handle: ObjectHandle) -> Result<ObjectInfo, Error> {
        self.backend.object_info(handle).await
    }

    // =========================================================================
    // Download operations
    // =========================================================================

    /// Download a whole file and return all bytes.
    ///
    /// For small to medium files where you want all the data in memory. For large files or
    /// streaming to disk, use [`download`](Self::download).
    pub async fn download_to_vec(&self, handle: ObjectHandle) -> Result<Vec<u8>, Error> {
        self.backend.read_range(handle, 0, None).await
    }

    /// Read a bounded byte range into a `Vec<u8>` (single shot, buffered).
    ///
    /// Uses the device's 64-bit partial-read operation, so offsets beyond 4 GB work on devices that
    /// advertise it. `len` is capped at `u32::MAX` per call.
    pub async fn read_range(
        &self,
        handle: ObjectHandle,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>, Error> {
        self.backend.read_range(handle, offset, Some(len)).await
    }

    /// Fetch the thumbnail image bytes for an object.
    pub async fn thumbnail(&self, handle: ObjectHandle) -> Result<Vec<u8>, Error> {
        self.backend.thumbnail(handle).await
    }

    /// Download a file as a stream (true streaming), holding the session for the whole file.
    ///
    /// Yields data chunks as they arrive without buffering the entire file in memory. This is the
    /// raw-speed path; it holds the device's one session open for the whole file (see [`download`]
    /// docs). For a long read where the device must stay responsive to other work, use
    /// [`download_windowed`](Self::download_windowed) instead.
    ///
    /// # Resume on forward-only-seek devices
    ///
    /// A [`ByteRange::From`]/[`ByteRange::Range`] resume assumes the device can seek to the offset
    /// cheaply. The Windows WPD backend's Pixel-class devices return `E_NOTIMPL` from `IStream::Seek`,
    /// so the backend reaches the offset by reading and discarding the prefix: a resume is O(offset)
    /// and re-streams every byte before the offset. Resuming near the end of a large file re-reads
    /// almost the whole file, so prefer a single in-order pass over many small offset resumes there.
    ///
    /// [`download`]: Self::download
    pub async fn download(
        &self,
        handle: ObjectHandle,
        range: ByteRange,
    ) -> Result<FileDownload, Error> {
        let dl = self.backend.download(handle, range).await?;
        Ok(FileDownload::new(dl.size, dl.body))
    }

    /// Read a large file as a sequence of bounded windows, **freeing the session between every
    /// window** so the device stays responsive.
    ///
    /// Each [`next_window()`](WindowedDownload::next_window) is a single bounded read that completes
    /// and releases the device. Between two `next_window()` calls a consumer can interleave other
    /// device work (service a pending folder listing, navigate, check a cancel flag) without
    /// aborting the read.
    ///
    /// `window_size` is the maximum bytes per window. [`DEFAULT_DOWNLOAD_WINDOW`] (8 MiB) is a
    /// documented suggestion; a `window_size` of 0 is clamped to 1.
    ///
    /// # Resume on forward-only-seek devices
    ///
    /// A windowed *resume* from an offset (`ByteRange::From`/`Range`) re-reads the skipped prefix on
    /// devices whose `IStream::Seek` is `E_NOTIMPL` (the Windows WPD backend's Pixel-class devices),
    /// making the first window after the offset O(offset). The session-freeing benefit between windows
    /// still holds, but starting deep into a large file pays a full re-read of the prefix first;
    /// prefer covering the file from the start (`ByteRange::Full`) where possible.
    pub async fn download_windowed(
        &self,
        handle: ObjectHandle,
        range: ByteRange,
        window_size: u32,
    ) -> Result<WindowedDownload, Error> {
        let size = self.backend.object_info(handle).await?.size;
        let offset = range.offset();
        if offset > size {
            return Err(Error::invalid_data(format!(
                "windowed download offset {offset} is past the object size {size}"
            )));
        }
        Ok(WindowedDownload::new(
            Arc::clone(&self.backend),
            handle,
            size,
            offset,
            window_size,
        ))
    }

    /// Read a large file in windows using the default window size
    /// ([`DEFAULT_DOWNLOAD_WINDOW`], 8 MiB), covering the whole file.
    pub async fn download_windowed_default(
        &self,
        handle: ObjectHandle,
    ) -> Result<WindowedDownload, Error> {
        self.download_windowed(handle, ByteRange::Full, DEFAULT_DOWNLOAD_WINDOW)
            .await
    }

    // =========================================================================
    // Upload operations
    // =========================================================================

    /// Upload a file from a stream.
    ///
    /// The data streams directly to the device in chunks; the protocol only needs the total size
    /// upfront (provided via `info`), not the whole file in memory.
    ///
    /// # Errors
    ///
    /// Returns [`UploadError`] on failure. Uploads are two-phase: the object is created (yielding a
    /// handle), then the bytes are streamed. If the data phase fails, the device may keep a partial
    /// object, and [`UploadError::partial`] carries its handle so you can [`delete`](Self::delete)
    /// it or retry the data phase to resume. The library does **not** auto-delete it.
    pub async fn upload<'a, S>(
        &'a self,
        parent: Option<ObjectHandle>,
        info: NewObjectInfo,
        data: S,
    ) -> Result<ObjectHandle, UploadError>
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin + Send + 'a,
    {
        self.backend
            .upload(self.id, parent, info, Box::pin(data), None)
            .await
    }

    /// Upload a file with a progress callback.
    ///
    /// Progress is reported as data is read from the stream. Return `ControlFlow::Break(())` from
    /// the callback to cancel the upload (which surfaces as [`Error::Cancelled`] in
    /// [`UploadError::source`]).
    pub async fn upload_with_progress<'a, S, F>(
        &'a self,
        parent: Option<ObjectHandle>,
        info: NewObjectInfo,
        data: S,
        on_progress: F,
    ) -> Result<ObjectHandle, UploadError>
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin + Send + 'a,
        F: FnMut(Progress) -> ControlFlow<()> + Send + 'a,
    {
        let progress: ProgressFn<'a> = Box::new(on_progress);
        self.backend
            .upload(self.id, parent, info, Box::pin(data), Some(progress))
            .await
    }

    // =========================================================================
    // Folder and object management
    // =========================================================================

    pub async fn create_folder(
        &self,
        parent: Option<ObjectHandle>,
        name: &str,
    ) -> Result<ObjectHandle, Error> {
        self.backend.create_folder(self.id, parent, name).await
    }

    pub async fn delete(&self, handle: ObjectHandle) -> Result<(), Error> {
        self.backend.delete(handle, None).await
    }

    /// Like [`delete`](Self::delete), but bails with `Err(Error::Cancelled)` before issuing the
    /// delete request when the token is set.
    pub async fn delete_with_cancel(
        &self,
        handle: ObjectHandle,
        cancel: Option<&CancelToken>,
    ) -> Result<(), Error> {
        bail_if_cancelled(cancel)?;
        self.backend.delete(handle, cancel).await
    }

    /// Move an object to a different folder (optionally a different storage).
    pub async fn move_object(
        &self,
        handle: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: Option<StorageId>,
    ) -> Result<(), Error> {
        let storage = new_storage.unwrap_or(self.id);
        self.backend.move_object(handle, new_parent, storage).await
    }

    pub async fn copy_object(
        &self,
        handle: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: Option<StorageId>,
    ) -> Result<ObjectHandle, Error> {
        let storage = new_storage.unwrap_or(self.id);
        self.backend.copy_object(handle, new_parent, storage).await
    }

    /// Rename an object (file or folder).
    ///
    /// Not all devices support renaming. Use `MtpDevice::supports_rename()` to check.
    pub async fn rename(&self, handle: ObjectHandle, new_name: &str) -> Result<(), Error> {
        self.backend.rename(handle, new_name).await
    }
}
