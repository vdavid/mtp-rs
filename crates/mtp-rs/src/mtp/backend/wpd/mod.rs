//! The Windows WPD-over-COM backend (`cfg(windows)`).
//!
//! Implements the backend-neutral [`MtpBackend`](crate::mtp::backend::MtpBackend) trait against the
//! Windows Portable Devices COM API. WPD is *not* a USB transport — it speaks MTP for us and exposes
//! a high-level object model — so this backend is a sibling to [`UsbBackend`](super::usb::UsbBackend),
//! not another `Transport`. See `docs/windows-wpd-backend-plan.md`.
//!
//! ## Threading
//!
//! WPD COM pointers are apartment-affine and `!Send`/`!Sync`. Rather than wrap them in `unsafe Send`,
//! one dedicated [`std::thread`] per open device (`actor.rs`) owns *all* the COM interface pointers,
//! `CoInitializeEx`es an MTA, and processes one request at a time off a channel. [`WpdBackend`] holds
//! only channel senders, so it is `Send + Sync` with **zero `unsafe`** in the public path.

mod actor;
mod com;
mod consts;
mod events;
mod ids;
mod props;

use crate::cancel::CancelToken;
use crate::mtp::backend::{
    BackendDownload, BackendListing, ByteRange, DownloadBody, MtpBackend, ProgressFn, UploadStream,
};
use crate::mtp::object::NewObjectInfo;
use crate::mtp::stream::Progress;
use crate::mtp::{
    Capabilities, DeviceEvent, DeviceInfo, Error, ObjectHandle, ObjectInfo, StorageId, StorageInfo,
    UploadError,
};
use actor::{OpenSpec, WpdHandle};
use async_trait::async_trait;
use bytes::Bytes;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use std::ops::ControlFlow;
use std::time::Duration;

/// The Windows WPD-over-COM backend. Holds only the worker handle (channel ends) plus the device's
/// identity/capabilities cached at open, so it is `Send + Sync` with no `unsafe`.
pub(crate) struct WpdBackend {
    handle: WpdHandle,
    device_info: DeviceInfo,
    capabilities: Capabilities,
    /// Device events delivered from the WPD callback over its own (separate from the request/reply)
    /// channel. Behind an async `Mutex` because `next_event(&self)` mutates the receiver and may be
    /// called repeatedly; the lock is held only across one `next()` await.
    events: futures::lock::Mutex<mpsc::UnboundedReceiver<DeviceEvent>>,
}

impl WpdBackend {
    /// Open the first WPD device Windows enumerates.
    pub(crate) async fn open_first() -> Result<Self, Error> {
        Self::spawn(OpenSpec::First).await
    }

    /// Open the WPD device whose serial number matches.
    pub(crate) async fn open_by_serial(serial: &str) -> Result<Self, Error> {
        Self::spawn(OpenSpec::Serial(serial.to_string())).await
    }

    /// Open the WPD device for an nusb USB device (correlates a `location_id`): VID/PID, then the
    /// USB serial to disambiguate identical models. See [`OpenSpec::UsbDevice`].
    pub(crate) async fn open_for_usb(
        serial: Option<String>,
        vid: u16,
        pid: u16,
    ) -> Result<Self, Error> {
        Self::spawn(OpenSpec::UsbDevice { serial, vid, pid }).await
    }

    async fn spawn(spec: OpenSpec) -> Result<Self, Error> {
        let (handle, device_info, capabilities, events) = WpdHandle::spawn(spec).await?;
        Ok(Self {
            handle,
            device_info,
            capabilities,
            events: futures::lock::Mutex::new(events),
        })
    }
}

/// The WPD streaming-download body: chunks arrive over a bounded channel from the worker thread.
struct WpdDownloadBody {
    data: mpsc::Receiver<Result<Bytes, Error>>,
}

#[async_trait]
impl DownloadBody for WpdDownloadBody {
    async fn next_chunk(&mut self) -> Option<Result<Bytes, Error>> {
        self.data.next().await
    }

    async fn cancel(&mut self, _idle_timeout: Duration) -> Result<(), Error> {
        // Close + drain the receiver: the worker's next `send` then fails, which stops its read
        // loop and releases the `IStream`. WPD cancel is "stop reading + Release" — no SIC drain.
        self.data.close();
        while self.data.next().await.is_some() {}
        Ok(())
    }
}

#[async_trait]
impl MtpBackend for WpdBackend {
    fn device_info(&self) -> &DeviceInfo {
        &self.device_info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn storages(&self) -> Result<Vec<StorageInfo>, Error> {
        self.handle.call(actor::Request::Storages).await
    }

    async fn storage_info(&self, storage: StorageId) -> Result<StorageInfo, Error> {
        self.handle
            .call(|reply| actor::Request::StorageInfo(storage, reply))
            .await
    }

    async fn list(
        &self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        cancel: Option<&CancelToken>,
    ) -> Result<BackendListing, Error> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(Error::Cancelled);
        }
        let objs = self
            .handle
            .call(|reply| actor::Request::List {
                storage,
                parent,
                cancel: cancel.cloned(),
                reply,
            })
            .await?;
        let total = objs.len();
        let items = futures::stream::iter(
            objs.into_iter()
                .map(Ok::<ObjectInfo, super::BackendListingError>),
        )
        .boxed();
        Ok(BackendListing { total, items })
    }

    async fn object_info(&self, obj: ObjectHandle) -> Result<ObjectInfo, Error> {
        self.handle
            .call(|reply| actor::Request::ObjectInfo(obj, reply))
            .await
    }

    async fn download(
        &self,
        obj: ObjectHandle,
        range: ByteRange,
    ) -> Result<BackendDownload, Error> {
        let start = self
            .handle
            .call(|reply| actor::Request::Download { obj, range, reply })
            .await?;
        Ok(BackendDownload {
            size: start.size,
            body: Box::new(WpdDownloadBody { data: start.data }),
        })
    }

    async fn read_range(
        &self,
        obj: ObjectHandle,
        offset: u64,
        len: Option<u32>,
    ) -> Result<Vec<u8>, Error> {
        self.handle
            .call(|reply| actor::Request::ReadRange {
                obj,
                offset,
                len,
                reply,
            })
            .await
    }

    async fn thumbnail(&self, obj: ObjectHandle) -> Result<Vec<u8>, Error> {
        // Reads the WPD_RESOURCE_THUMBNAIL resource on the worker. Objects with no thumbnail fail at
        // GetStream and surface as Unsupported/NotFound.
        self.handle
            .call(|reply| actor::Request::Thumbnail { obj, reply })
            .await
    }

    // ---- write path (Phase 3) ------------------------------------------------------------------

    async fn upload(
        &self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        info: NewObjectInfo,
        mut data: UploadStream<'_>,
        mut progress: Option<ProgressFn<'_>>,
    ) -> Result<ObjectHandle, UploadError> {
        // Stream the source straight to the worker over a bounded channel: each chunk is written to
        // the device as it arrives, so peak memory is a few in-flight chunks, not the file size. A
        // slow device back-pressures the source via the bounded `send`. The object is created BEFORE
        // all bytes arrive (incremental writes), so a mid-stream abort can leave a partial — the
        // worker probes for it and reports it in `UploadReply::ShortClosed { partial }`.
        let total = info.size;
        let (mut tx, rx) = mpsc::channel::<Bytes>(actor::DATA_BOUND);
        let (reply_tx, reply_rx) = futures::channel::oneshot::channel::<actor::UploadReply>();
        self.handle
            .send(actor::Request::Upload {
                storage,
                parent,
                info,
                data: rx,
                reply: reply_tx,
            })
            .map_err(|source| UploadError {
                source,
                partial: None,
            })?;

        // Why the source stopped, so the worker's device verdict can be reconciled with it.
        enum Stop {
            /// Source ended cleanly (saw `None`).
            Clean,
            /// The progress callback returned `Break`.
            Cancelled,
            /// The source stream yielded an error.
            Source(std::io::Error),
        }

        let mut forwarded: u64 = 0;
        let stop = loop {
            match data.next().await {
                None => break Stop::Clean,
                Some(Err(e)) => break Stop::Source(e),
                Some(Ok(chunk)) => {
                    let next = forwarded + chunk.len() as u64;
                    if let Some(cb) = progress.as_mut() {
                        let p = Progress {
                            bytes_transferred: next,
                            total_bytes: Some(total),
                        };
                        if let ControlFlow::Break(()) = cb(p) {
                            break Stop::Cancelled;
                        }
                    }
                    if tx.send(chunk).await.is_err() {
                        // Worker dropped the receiver (it errored or finished early); its reply has
                        // the verdict, so stop driving and go read it.
                        break Stop::Clean;
                    }
                    forwarded = next;
                }
            }
        };
        // Closing the sender ends the upload for the worker: clean at `info.size` → commit, else abort.
        drop(tx);

        let reply = reply_rx.await.map_err(|_| UploadError {
            source: Error::Disconnected,
            partial: None,
        })?;
        match (stop, reply) {
            // A device create/write error always wins (no partial committed).
            (_, actor::UploadReply::Error(source)) => Err(UploadError {
                source,
                partial: None,
            }),
            // All bytes made it and the device committed — success regardless of why the source loop
            // ended (a `Break`/source-error observed only after the final chunk was already sent).
            (_, actor::UploadReply::Committed(handle)) => Ok(handle),
            // Short close from a cancel: report Cancelled with whatever partial the device kept.
            (Stop::Cancelled, actor::UploadReply::ShortClosed { partial }) => Err(UploadError {
                source: Error::Cancelled,
                partial,
            }),
            // Short close from a source error: report the I/O error with the partial.
            (Stop::Source(e), actor::UploadReply::ShortClosed { partial }) => Err(UploadError {
                source: Error::Io {
                    message: e.to_string(),
                },
                partial,
            }),
            // Source ended cleanly but short of the declared size: a truncated upload.
            (Stop::Clean, actor::UploadReply::ShortClosed { partial }) => Err(UploadError {
                source: Error::invalid_data(
                    "upload stream ended before the declared size was reached",
                ),
                partial,
            }),
        }
    }

    async fn create_folder(
        &self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        name: &str,
    ) -> Result<ObjectHandle, Error> {
        let name = name.to_string();
        self.handle
            .call(|reply| actor::Request::CreateFolder {
                storage,
                parent,
                name,
                reply,
            })
            .await
    }

    async fn delete(&self, obj: ObjectHandle, cancel: Option<&CancelToken>) -> Result<(), Error> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(Error::Cancelled);
        }
        self.handle
            .call(|reply| actor::Request::Delete { obj, reply })
            .await
    }

    async fn move_object(
        &self,
        obj: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: StorageId,
    ) -> Result<(), Error> {
        self.handle
            .call(|reply| actor::Request::MoveObject {
                obj,
                new_parent,
                new_storage,
                reply,
            })
            .await
    }

    async fn copy_object(
        &self,
        obj: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: StorageId,
    ) -> Result<ObjectHandle, Error> {
        self.handle
            .call(|reply| actor::Request::CopyObject {
                obj,
                new_parent,
                new_storage,
                reply,
            })
            .await
    }

    async fn rename(&self, obj: ObjectHandle, new_name: &str) -> Result<(), Error> {
        let name = new_name.to_string();
        self.handle
            .call(|reply| actor::Request::Rename { obj, name, reply })
            .await
    }

    async fn next_event(&self) -> Result<DeviceEvent, Error> {
        // Block until the WPD callback delivers the next event. A closed channel means the worker
        // (and its event sender) is gone, i.e. the device session ended. The caller wraps this in a
        // timeout if it wants one; we wait indefinitely.
        let mut events = self.events.lock().await;
        events.next().await.ok_or(Error::Disconnected)
    }

    async fn close(&self) -> Result<(), Error> {
        self.handle.shutdown();
        Ok(())
    }
}
