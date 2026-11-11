//! The backend seam for the high-level [`crate::mtp`] API.
//!
//! [`MtpBackend`] is the one abstraction every concrete portable-device backend implements in
//! backend-neutral vocabulary (neutral [`crate::mtp`] types and [`crate::mtp::Error`]). The
//! implementations are the PTP-over-USB backend ([`UsbBackend`](usb::UsbBackend)) and, on Windows, the WPD-over-COM
//! backend (`wpd::WpdBackend`). [`crate::mtp::MtpDevice`] and
//! [`crate::mtp::Storage`] are thin concrete façades over a `Box<dyn MtpBackend>`, so consumers
//! never see the trait or generics.
//!
//! A trait (not enum dispatch) keeps each backend self-contained in its own module and makes a
//! future backend a new file rather than edits to every method; the per-call dynamic dispatch is
//! noise against USB/COM latency.

pub(crate) mod usb;

#[cfg(windows)]
pub(crate) mod wpd;

// The WPD backend's OLE date conversion is pure Rust, so its tests also run off Windows.
#[cfg(all(test, not(windows)))]
#[path = "wpd/ole_date.rs"]
mod wpd_ole_date;

use crate::cancel::CancelToken;
use crate::mtp::object::NewObjectInfo;
use crate::mtp::stream::Progress;
use crate::mtp::{
    Capabilities, DeviceEvent, DeviceInfo, Error, ObjectHandle, ObjectInfo, StorageId, StorageInfo,
    UploadError,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::time::Duration;

/// Selects which backend [`MtpDeviceBuilder`](crate::mtp::MtpDeviceBuilder) opens.
///
/// `Auto` (the default) picks per platform: on Windows it prefers the WPD backend (phones are bound
/// to the WPD driver, not WinUSB), falling back to PTP-over-USB only if no WPD device is present;
/// on other platforms it uses USB. `Usb` forces PTP-over-USB (e.g. a Zadig/WinUSB-bound camera on
/// Windows); `Wpd` forces Windows WPD-over-COM (and errors as unsupported off Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Backend {
    /// Platform default: Windows → WPD then USB; elsewhere → USB.
    #[default]
    Auto,
    /// Force PTP-over-USB.
    Usb,
    /// Force Windows WPD-over-COM.
    Wpd,
}

/// Which bytes of an object a download should cover.
///
/// Used by the backend's download / read-range primitives to express whole-file, resume, and
/// bounded-window reads with one type. The façade's download conveniences all desugar to this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ByteRange {
    /// The whole object, `[0, size)`.
    Full,
    /// From `offset` to end-of-file, `[offset, size)`.
    From(u64),
    /// A bounded slice `[offset, offset + len)` (clamped to the object size by the backend).
    Range {
        /// Start byte offset.
        offset: u64,
        /// Number of bytes.
        len: u64,
    },
}

impl ByteRange {
    /// The starting byte offset of this range.
    #[must_use]
    pub(crate) fn offset(self) -> u64 {
        match self {
            ByteRange::Full => 0,
            ByteRange::From(offset) | ByteRange::Range { offset, .. } => offset,
        }
    }
}

/// A progress callback for uploads. Returning [`ControlFlow::Break`] cancels the transfer.
///
/// Lifetime-parameterized (not `'static`) so a consumer can pass a callback that borrows local
/// state — the upload is awaited to completion within the call, so the borrow need only outlive
/// that call.
pub(crate) type ProgressFn<'a> = Box<dyn FnMut(Progress) -> ControlFlow<()> + Send + 'a>;

/// A boxed, backend-neutral stream of upload data chunks.
pub(crate) type UploadStream<'a> =
    Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + 'a>>;

/// One in-progress streamed read (a held-open download or one window of a paced one), holding
/// whatever resource the backend needs until it ends (the USB backend holds the PTP session). The
/// façade's [`crate::mtp::FileDownload`] wraps these; consumers don't see them.
#[async_trait]
pub(crate) trait DownloadBody: Send {
    /// The next chunk of data, or `None` at the end of this read. The resource is released by the
    /// time `None` (or an error) comes back.
    async fn next_chunk(&mut self) -> Option<Result<Bytes, Error>>;

    /// Cancel the in-flight read, releasing the backend resource and leaving the device usable
    /// for the next operation. A no-op if already complete.
    async fn cancel(&mut self, idle_timeout: Duration) -> Result<(), Error>;

    /// How long the device took to send this read's bytes, once it has ended, when the backend
    /// measures that itself. `None` means [`FileDownload`](crate::mtp::FileDownload) times its own
    /// waits for chunks, which is right only when the body reads on demand. WPD's worker reads
    /// ahead into a channel while the consumer handles earlier chunks, so there those waits
    /// undercount the device's time and the pacer would size windows far past the target.
    fn device_time(&self) -> Option<Duration> {
        None
    }
}

/// An object being written in windows, opened by [`MtpBackend::begin_windowed_upload`]. The
/// façade's paced upload drives it; consumers don't see it.
///
/// It tracks how many bytes the device acknowledged and writes only there, so no window can ever
/// start past the end of what the device holds (Android refuses such a window without reading
/// its data, which wedges the phone). Dropping it with the edit still open makes the next
/// operation on the device end the edit and delete the object, since nobody holds its handle;
/// after [`keep_on_drop`](Self::keep_on_drop) it only ends the edit.
#[async_trait]
pub(crate) trait UploadBody: Send {
    /// The new object's handle.
    fn handle(&self) -> ObjectHandle;

    /// Bytes the device acknowledged so far: where the next window starts.
    fn acknowledged(&self) -> u64;

    /// Send the next `len` bytes from `data`, at [`acknowledged`](Self::acknowledged), and hand
    /// the body back with the result. Owned (`'static`) so the window can stay in flight across
    /// calls of a [`FileUpload`](crate::mtp::FileUpload); dropping the future drops the body. On
    /// an error the acknowledged size doesn't move.
    fn write_window(
        self: Box<Self>,
        len: u32,
        data: UploadStream<'static>,
    ) -> futures::future::BoxFuture<'static, (Box<dyn UploadBody>, Result<(), Error>)>;

    /// The caller is getting the handle (a stop that returns `partial`), so a later drop with the
    /// edit still open ends the edit but keeps the object.
    fn keep_on_drop(&mut self);

    /// Commit the object: the upload is complete.
    async fn finish(&mut self) -> Result<(), Error>;

    /// Give up: cut the object back to the acknowledged bytes (a stopped window can leave some of
    /// its bytes on disk past them) and end the edit, so the device keeps a clean prefix.
    async fn abandon(&mut self) -> Result<(), Error>;
}

/// Whether a per-handle metadata error invalidates the whole collection or can
/// be reported while sibling enumeration continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListingErrorDisposition {
    Fatal,
    SkipObject,
}

/// One error produced after a backend has already enumerated an object's
/// handle. Keeping the handle and disposition internal lets the public MTP API
/// remain backend-neutral while detailed collection callers still receive a
/// useful diagnostic.
#[derive(Debug)]
pub(crate) struct BackendListingError {
    pub(crate) handle: ObjectHandle,
    pub(crate) source: Error,
    pub(crate) disposition: ListingErrorDisposition,
}

impl BackendListingError {
    pub(crate) fn fatal(handle: ObjectHandle, source: Error) -> Self {
        Self {
            handle,
            source,
            disposition: ListingErrorDisposition::Fatal,
        }
    }

    pub(crate) fn skippable(handle: ObjectHandle, source: Error) -> Self {
        Self {
            handle,
            source,
            disposition: ListingErrorDisposition::SkipObject,
        }
    }
}

/// A boxed stream of object metadata, yielded by [`MtpBackend::list`].
pub(crate) type ObjectStream =
    Pin<Box<dyn Stream<Item = Result<ObjectInfo, BackendListingError>> + Send>>;

/// One in-progress listing returned by [`MtpBackend::list`]: a known total plus the item stream.
pub(crate) struct BackendListing {
    /// Total number of handles the device reported (before any parent filtering).
    pub(crate) total: usize,
    /// The metadata stream.
    pub(crate) items: ObjectStream,
}

/// The backend-neutral portable-device API. See the module docs.
#[async_trait]
pub(crate) trait MtpBackend: Send + Sync {
    /// Cached device identity.
    fn device_info(&self) -> &DeviceInfo;

    /// What the device supports (derived per backend).
    fn capabilities(&self) -> &Capabilities;

    /// All storages on the device.
    async fn storages(&self) -> Result<Vec<StorageInfo>, Error>;

    /// Fetch info for a single storage by id.
    async fn storage_info(&self, storage: StorageId) -> Result<StorageInfo, Error>;

    /// List the children of `parent` on `storage` (cancellable, streaming).
    async fn list(
        &self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        cancel: Option<&CancelToken>,
    ) -> Result<BackendListing, Error>;

    /// Metadata for one object (with the full >4 GB size resolved).
    async fn object_info(&self, obj: ObjectHandle) -> Result<ObjectInfo, Error>;

    /// One held-open streaming download of `obj` over `range`, holding the backend resource for the
    /// whole transfer. `size` is the object's full size, which the caller already looked up.
    async fn download(
        &self,
        obj: ObjectHandle,
        range: ByteRange,
        size: u64,
    ) -> Result<Box<dyn DownloadBody>, Error>;

    /// One streamed partial read of `len` bytes from `offset`: a single window of a paced download.
    /// The backend resource is held only until this body ends.
    async fn download_window(
        &self,
        obj: ObjectHandle,
        offset: u64,
        len: u32,
    ) -> Result<Box<dyn DownloadBody>, Error>;

    /// Whether a partial read can start at `offset`. False when the device has no partial-read
    /// operation, or only the 32-bit one and `offset` is past 4 GiB.
    fn can_read_partially_at(&self, offset: u64) -> bool;

    /// Called between the windows of a paced transfer, with the device free: let a call that
    /// queued for the device during the last window run before the next window starts. USB
    /// waits until a queued call has the session lock, since a yield alone doesn't do that on a
    /// multi-thread runtime (`ptp/session/lock.rs`). The default yields once, which is enough
    /// where requests queue in order (WPD's worker channel).
    async fn let_queued_calls_in(&self) {
        crate::mtp::stream::YieldNow::default().await;
    }

    /// A single-shot buffered read of `[offset, offset+len)` (or to EOF when `len` is `None`).
    async fn read_range(
        &self,
        obj: ObjectHandle,
        offset: u64,
        len: Option<u32>,
    ) -> Result<Vec<u8>, Error>;

    /// Fetch the thumbnail bytes for `obj`.
    async fn thumbnail(&self, obj: ObjectHandle) -> Result<Vec<u8>, Error>;

    /// Create a new object under `parent` on `storage` and stream its data.
    async fn upload(
        &self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        info: NewObjectInfo,
        data: UploadStream<'_>,
        progress: Option<ProgressFn<'_>>,
    ) -> Result<ObjectHandle, UploadError>;

    /// Whether uploads can go out in windows that free the device between them (the USB backend
    /// on a device listing Android's edit operations, unless the consumer turned it off).
    fn can_upload_in_windows(&self) -> bool {
        false
    }

    /// The pacing `Storage::upload` uses: what the consumer chose with
    /// [`MtpDeviceBuilder::upload_pacing`](crate::mtp::MtpDeviceBuilder::upload_pacing), else
    /// held open.
    fn default_upload_pacing(&self) -> crate::mtp::UploadPacing {
        crate::mtp::UploadPacing::HeldOpen
    }

    /// Create an empty object under `parent` on `storage` and open it for a windowed upload.
    /// Only called when [`can_upload_in_windows`](Self::can_upload_in_windows) is true. A failure
    /// after the object exists carries its handle in [`UploadError::partial`].
    async fn begin_windowed_upload(
        &self,
        _storage: StorageId,
        _parent: Option<ObjectHandle>,
        _info: &NewObjectInfo,
    ) -> Result<Box<dyn UploadBody>, UploadError> {
        Err(UploadError {
            source: Error::Unsupported,
            partial: None,
        })
    }

    /// Create a folder named `name` under `parent` on `storage`.
    async fn create_folder(
        &self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        name: &str,
    ) -> Result<ObjectHandle, Error>;

    /// Delete `obj` (cancellable before the request is issued).
    async fn delete(&self, obj: ObjectHandle, cancel: Option<&CancelToken>) -> Result<(), Error>;

    /// Move `obj` to `new_parent` (optionally on `new_storage`).
    async fn move_object(
        &self,
        obj: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: StorageId,
    ) -> Result<(), Error>;

    /// Copy `obj` to `new_parent` (optionally on `new_storage`), returning the copy's handle.
    async fn copy_object(
        &self,
        obj: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: StorageId,
    ) -> Result<ObjectHandle, Error>;

    /// Rename `obj`.
    async fn rename(&self, obj: ObjectHandle, new_name: &str) -> Result<(), Error>;

    /// Await the next device event (indefinitely; wrap in a timeout).
    async fn next_event(&self) -> Result<DeviceEvent, Error>;

    /// Close the connection (best-effort). Also happens on drop of the backing resource.
    async fn close(&self) -> Result<(), Error>;
}
