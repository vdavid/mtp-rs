//! The backend seam for the high-level [`crate::mtp`] API.
//!
//! [`MtpBackend`] is the one abstraction every concrete portable-device backend implements in
//! backend-neutral vocabulary (neutral [`crate::mtp`] types and [`crate::mtp::Error`]). The
//! PTP-over-USB backend ([`UsbBackend`]) is the sole implementation today; a Windows WPD-over-COM
//! backend is planned (see `docs/windows-wpd-backend-plan.md`). [`crate::mtp::MtpDevice`] and
//! [`crate::mtp::Storage`] are thin concrete façades over a `Box<dyn MtpBackend>`, so consumers
//! never see the trait or generics.
//!
//! A trait (not enum dispatch) keeps each backend self-contained in its own module and makes a
//! future backend a new file rather than edits to every method; the per-call dynamic dispatch is
//! noise against USB/COM latency.

pub(crate) mod usb;

#[cfg(windows)]
pub(crate) mod wpd;

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

/// One in-progress streaming download, holding whatever resource the backend needs for the whole
/// transfer (the USB backend holds the PTP session open). The façade's [`crate::mtp::FileDownload`]
/// wraps this; consumers don't see it.
#[async_trait]
pub(crate) trait DownloadBody: Send {
    /// The next chunk of data, or `None` at end-of-file.
    async fn next_chunk(&mut self) -> Option<Result<Bytes, Error>>;

    /// Cancel the in-flight download, releasing the backend resource and leaving the device usable
    /// for the next operation. A no-op if already complete.
    async fn cancel(&mut self, idle_timeout: Duration) -> Result<(), Error>;
}

/// One streaming download returned by [`MtpBackend::download`]: the full object size plus the body.
pub(crate) struct BackendDownload {
    /// Full object size in bytes (always the whole file, even for an offset/range read).
    pub(crate) size: u64,
    /// The download body.
    pub(crate) body: Box<dyn DownloadBody>,
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

    /// A streaming download of `obj` over `range`. Holds the backend resource for the transfer.
    async fn download(&self, obj: ObjectHandle, range: ByteRange)
        -> Result<BackendDownload, Error>;

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
