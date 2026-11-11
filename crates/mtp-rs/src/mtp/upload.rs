//! Uploads, step by step: [`FileUpload`], and the pacing that spreads a file over device
//! transactions.
//!
//! The policy lives here, backend-neutral, the way paced downloads live in [`super::stream`]:
//! window sizes come from the same `Pacer`, and the window loop, letting queued calls in between windows, and
//! what a stop does are decided once for every backend. A backend provides
//! [`MtpBackend::upload`] (one held-open transfer) and, where it can,
//! [`MtpBackend::begin_windowed_upload`] and the [`UploadBody`] it returns (today the USB backend,
//! through Android's edit operations). Why, and the hardware numbers: AGENTS.md § Paced uploads
//! and `docs/specs/paced-uploads.md`.
//!
//! Both kinds of transfer read the caller's chunks from a channel, so a transfer in flight can
//! outlive one [`FileUpload::write_chunk`] call and wait for the next, the way a download's window
//! waits between [`FileDownload::next_chunk`](crate::FileDownload::next_chunk) calls.

use crate::mtp::backend::{MtpBackend, ProgressFn, UploadBody, UploadStream};
use crate::mtp::stream::Pacer;
use crate::mtp::{
    DownloadPacing, Error, NewObjectInfo, ObjectHandle, Progress, StorageId, UploadError,
};
use bytes::Bytes;
use futures::channel::mpsc;
use futures::future::{BoxFuture, Either};
use futures::{FutureExt, SinkExt, StreamExt};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long each window of [`UploadPacing::MaxWindowTime`] takes, about, at the suggested
/// setting: 2 seconds.
pub const DEFAULT_UPLOAD_WINDOW_TIME: Duration = Duration::from_secs(2);

/// How an upload spreads a file over device transactions.
///
/// The device serves one operation at a time. One held-open `SendObject` holds it for the whole
/// file, so a folder listing from another task waits for the rest of the upload. On a device that
/// supports it ([`Capabilities::supports_windowed_upload`](crate::Capabilities::supports_windowed_upload),
/// which Android phones list), the file can go out as a series of windows instead, and the device
/// is free between them: another call waits for one window at most. Each window costs about 1–3 ms
/// on top of its data (Galaxy S23 Ultra, Pixel 9 Pro XL), so windows of about two seconds cost
/// well under 1%.
///
/// **Windows lose the file's modification date on Android**: the phone dates the file when it's
/// created and every window's write moves the date on, so the file ends up dated at its last
/// window, and Android can't set it afterwards. A held-open upload keeps the date on a Pixel. So
/// the default is [`HeldOpen`](Self::HeldOpen), and windows are opt-in: per upload through
/// [`Storage::upload_with_pacing`](crate::Storage::upload_with_pacing) or
/// [`Storage::start_upload`](crate::Storage::start_upload), or for a whole device through
/// [`MtpDeviceBuilder::upload_pacing`](crate::MtpDeviceBuilder::upload_pacing).
///
/// Devices without the operations, and the Windows WPD backend, always upload in one piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum UploadPacing {
    /// One `SendObject` for the whole file, holding the device until it ends. The default.
    #[default]
    HeldOpen,
    /// Windows sized from the measured speed to take about this long, so another call waits at
    /// most about this long. The first window is 8 MiB; later ones are never smaller than 1 MiB.
    /// [`DEFAULT_UPLOAD_WINDOW_TIME`] (2 s) is the suggested setting.
    MaxWindowTime(Duration),
    /// Windows of a fixed size in bytes (0 counts as 1).
    FixedWindow(u32),
}

impl UploadPacing {
    /// The same window sizing, in the download pacer's terms.
    fn sizing(self) -> DownloadPacing {
        match self {
            UploadPacing::HeldOpen => DownloadPacing::HeldOpen,
            UploadPacing::MaxWindowTime(target) => DownloadPacing::MaxCancelDelay(target),
            UploadPacing::FixedWindow(size) => DownloadPacing::FixedWindow(size),
        }
    }
}

type Chunk = std::io::Result<Bytes>;
type HeldFuture = BoxFuture<'static, Result<ObjectHandle, UploadError>>;
type WindowFuture = BoxFuture<'static, (Box<dyn UploadBody>, Result<(), Error>)>;

/// A transfer reading the caller's chunks from `tx`.
struct InFlight<F> {
    future: F,
    tx: mpsc::Sender<Chunk>,
}

impl<F: std::future::Future + Unpin> InFlight<F> {
    /// Hand `chunk` to the transfer, running it until it takes the chunk. `Err` carries the
    /// transfer's result when it ended before taking it (an error, or a device that answered
    /// early).
    async fn feed(&mut self, chunk: Chunk) -> Result<(), F::Output> {
        let send = self.tx.send(chunk);
        futures::pin_mut!(send);
        match futures::future::select(&mut self.future, send).await {
            Either::Left((output, _)) => Err(output),
            Either::Right(_) => Ok(()),
        }
    }

    /// Tell the transfer its data is over and wait for it to end.
    async fn close(mut self) -> F::Output {
        self.tx.close_channel();
        self.future.await
    }
}

/// The two ways an upload goes out.
enum Mode {
    /// One `MtpBackend::upload` for the whole file, started by the first chunk (or `finish`).
    Held {
        backend: Arc<dyn MtpBackend>,
        start: Option<(StorageId, Option<ObjectHandle>, NewObjectInfo)>,
        transfer: Option<InFlight<HeldFuture>>,
    },
    /// Windows into an object opened for editing.
    Windowed {
        /// For [`MtpBackend::let_queued_calls_in`] between windows.
        backend: Arc<dyn MtpBackend>,
        /// The open object while no window is in flight (a window owns it while it runs).
        body: Option<Box<dyn UploadBody>>,
        handle: ObjectHandle,
        window: Option<Window>,
        pacer: Pacer,
    },
}

/// One window in flight.
struct Window {
    transfer: InFlight<WindowFuture>,
    len: u32,
    /// Bytes of this window not handed over yet.
    left: u64,
    started: Instant,
}

/// An upload in progress, fed one chunk at a time.
///
/// Created by [`Storage::start_upload`](crate::Storage::start_upload). The caller hands over the
/// file's bytes with [`write_chunk`](Self::write_chunk), in pieces of any size, then calls
/// [`finish`](Self::finish). Exactly [`size`](Self::size) bytes have to arrive: fewer and
/// `finish` fails, more and `write_chunk` does.
///
/// # Windows and the device lock
///
/// With a windowed [`UploadPacing`] on a device that supports it, the file goes out in windows,
/// and the device is free between them. [`holds_session`](Self::holds_session) says when a
/// transfer is in flight: a consumer with its own lock around the device holds it while that's
/// `true` and lets other work in while it's `false`. A window opens with the first chunk after
/// the last one ended and closes in the `write_chunk` call that hands over its last byte, so
/// calls on the device run straight after that call returns. A held-open upload holds the device
/// from its first chunk to `finish`. While `holds_session()` is `true` the device is waiting
/// mid-transfer for the next chunk, so keep them coming (and don't wait on a lock that a call on
/// this device holds: it can't run until the transfer ends).
///
/// # Stopping
///
/// [`cancel`](Self::cancel) stops the upload. A windowed upload keeps exactly the windows the
/// device acknowledged, and `cancel` returns that object's handle; the library never deletes it.
/// A held-open upload to Android leaves nothing. Dropping a `FileUpload` stops it too, but nobody
/// learns the handle, so a windowed one is deleted by the next operation on the device (`Drop`
/// can't send anything): a file cut short under its real name would read as a complete one.
///
/// # Example
///
/// ```rust,no_run
/// use mtp_rs::{NewObjectInfo, UploadPacing, DEFAULT_UPLOAD_WINDOW_TIME};
/// # async fn example(storage: &mtp_rs::Storage, data: Vec<bytes::Bytes>) -> Result<(), mtp_rs::Error> {
/// let size = data.iter().map(|c| c.len() as u64).sum();
/// let pacing = UploadPacing::MaxWindowTime(DEFAULT_UPLOAD_WINDOW_TIME);
/// let mut upload = storage
///     .start_upload(None, NewObjectInfo::file("movie.mp4", size), pacing)
///     .await?;
/// for chunk in data {
///     upload.write_chunk(chunk).await?;
///     if !upload.holds_session() {
///         // Between windows: other calls on the device run right away.
///     }
/// }
/// let handle = upload.finish().await?;
/// # Ok(())
/// # }
/// ```
#[must_use = "a FileUpload does nothing until you write to it and call finish()"]
pub struct FileUpload {
    mode: Mode,
    size: u64,
    /// Bytes the caller handed over.
    sent: u64,
    done: bool,
}

impl FileUpload {
    /// Set up an upload. A windowed one creates the object and opens it for editing here, so
    /// its errors surface at the call; a held-open one starts with the first chunk.
    pub(crate) async fn start(
        backend: Arc<dyn MtpBackend>,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        info: NewObjectInfo,
        pacing: UploadPacing,
    ) -> Result<Self, UploadError> {
        let size = info.size;
        // An empty file has no data to window: the held-open path is the same two operations.
        let windowed =
            pacing != UploadPacing::HeldOpen && size > 0 && backend.can_upload_in_windows();
        let mode = if windowed {
            let body = backend
                .begin_windowed_upload(storage, parent, &info)
                .await?;
            Mode::Windowed {
                backend,
                handle: body.handle(),
                body: Some(body),
                window: None,
                pacer: Pacer::new(pacing.sizing()),
            }
        } else {
            Mode::Held {
                backend,
                start: Some((storage, parent, info)),
                transfer: None,
            }
        };
        Ok(Self {
            mode,
            size,
            sent: 0,
            done: false,
        })
    }

    /// The file's size, as declared in its [`NewObjectInfo`].
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Bytes handed to [`write_chunk`](Self::write_chunk) so far.
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.sent
    }

    /// The new object's handle, once the device has created it: from the start for a windowed
    /// upload, and only from [`finish`](Self::finish) for a held-open one.
    #[must_use]
    pub fn handle(&self) -> Option<ObjectHandle> {
        match &self.mode {
            Mode::Windowed { handle, .. } => Some(*handle),
            Mode::Held { .. } => None,
        }
    }

    /// Whether the upload goes out in windows (the pacing asked for it and the device can).
    #[must_use]
    pub fn is_windowed(&self) -> bool {
        matches!(self.mode, Mode::Windowed { .. })
    }

    /// Whether a transfer is in flight right now, holding the device's one session.
    ///
    /// While it's `false` (between windows, before a held-open upload's first chunk, and once the
    /// upload is over), other calls on the device run right away. A consumer with its own device
    /// lock can hold it while this is `true` and let other work in when it's `false`.
    #[must_use]
    pub fn holds_session(&self) -> bool {
        match &self.mode {
            Mode::Held { transfer, .. } => transfer.is_some(),
            Mode::Windowed { window, .. } => window.is_some(),
        }
    }

    /// Hand over the next bytes of the file.
    ///
    /// # Errors
    ///
    /// On an error the upload is over: the device or the data failed, and
    /// [`UploadError::partial`] names what's left on the device, as for
    /// [`Storage::upload`](crate::Storage::upload). Handing over more than [`size`](Self::size)
    /// bytes is an [`Error::InvalidData`].
    pub async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), UploadError> {
        if self.done {
            return Err(UploadError {
                source: Error::invalid_data("the upload is already over"),
                partial: None,
            });
        }
        if self.sent + chunk.len() as u64 > self.size {
            let e = Error::invalid_data(format!(
                "the upload's data is longer than the {} bytes its NewObjectInfo declared",
                self.size
            ));
            return Err(self.fail(e).await);
        }
        self.sent += chunk.len() as u64;
        let result = if matches!(self.mode, Mode::Held { .. }) {
            self.feed_held(chunk).await
        } else {
            self.feed_windows(chunk).await
        };
        if result.is_err() {
            self.done = true;
        }
        result
    }

    /// Finish the upload once all [`size`](Self::size) bytes are written, and return the new
    /// object's handle.
    ///
    /// # Errors
    ///
    /// As [`write_chunk`](Self::write_chunk). Fewer bytes than declared is an error that stops
    /// the upload, like a source that ended early.
    pub async fn finish(mut self) -> Result<ObjectHandle, UploadError> {
        if self.done {
            return Err(UploadError {
                source: Error::invalid_data("the upload is already over"),
                partial: None,
            });
        }
        if self.sent < self.size {
            let e = Error::Io {
                message: format!(
                    "the upload's data ended at {} bytes, short of the {} its NewObjectInfo \
                     declared",
                    self.sent, self.size
                ),
            };
            return Err(self.fail(e).await);
        }
        self.done = true;
        match &mut self.mode {
            Mode::Held {
                backend,
                start,
                transfer,
            } => {
                // An empty file never got a chunk to start it.
                if transfer.is_none() {
                    *transfer = Some(start_held(backend, start.take()));
                }
                transfer.take().expect("started above").close().await
            }
            Mode::Windowed { body, handle, .. } => {
                // The last window closed in the call that handed over its last byte.
                let mut body = body.take().expect("no window is in flight at the end");
                match body.finish().await {
                    Ok(()) => Ok(*handle),
                    Err(source) => {
                        body.keep_on_drop();
                        Err(UploadError {
                            source,
                            partial: Some(*handle),
                        })
                    }
                }
            }
        }
    }

    /// Stop the upload and return what's left on the device: for a windowed upload, its object
    /// with exactly the windows the device acknowledged; for a held-open one, whatever the device
    /// kept (nothing on Android). The library never deletes it.
    pub async fn cancel(mut self) -> Option<ObjectHandle> {
        if self.done {
            return None;
        }
        let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled");
        self.fail_with(interrupted, Error::Cancelled).await.partial
    }

    /// Stop on a host-side error (`source`), as if the caller's data failed.
    async fn fail(&mut self, source: Error) -> UploadError {
        let io = std::io::Error::other(source.to_string());
        self.fail_with(io, source).await
    }

    /// Stop the upload: end a transfer in flight with `io` as its data's error, then clean up.
    async fn fail_with(&mut self, io: std::io::Error, source: Error) -> UploadError {
        self.done = true;
        match &mut self.mode {
            Mode::Held { transfer, .. } => {
                let Some(mut transfer) = transfer.take() else {
                    // Nothing was created yet.
                    return UploadError {
                        source,
                        partial: None,
                    };
                };
                // The backend maps a source error to the same outcome as before: Android drops
                // the object (`partial` `None`), other devices may keep it.
                let result = match transfer.feed(Err(io)).await {
                    Err(result) => result,
                    Ok(()) => transfer.close().await,
                };
                match result {
                    Ok(handle) => UploadError {
                        source,
                        partial: Some(handle),
                    },
                    Err(e) => UploadError {
                        source,
                        partial: e.partial,
                    },
                }
            }
            Mode::Windowed {
                body,
                handle,
                window,
                ..
            } => {
                let handle = *handle;
                if let Some(mut w) = window.take() {
                    let (returned, _) = match w.transfer.feed(Err(io)).await {
                        Err(output) => output,
                        Ok(()) => w.transfer.close().await,
                    };
                    *body = Some(returned);
                }
                let body = body
                    .take()
                    .expect("the body is back once no window is in flight");
                abandon(body, &source).await;
                UploadError {
                    source,
                    partial: Some(handle),
                }
            }
        }
    }

    async fn feed_held(&mut self, chunk: Bytes) -> Result<(), UploadError> {
        let Mode::Held {
            backend,
            start,
            transfer,
        } = &mut self.mode
        else {
            unreachable!("called for the held-open mode")
        };
        if chunk.is_empty() {
            return Ok(());
        }
        let transfer = transfer.get_or_insert_with(|| start_held(backend, start.take()));
        match transfer.feed(Ok(chunk)).await {
            Ok(()) => Ok(()),
            // The upload ended before it took all the data.
            Err(Ok(_)) => Err(UploadError {
                source: Error::invalid_data("the device finished the upload before its data ended"),
                partial: None,
            }),
            Err(Err(e)) => Err(e),
        }
    }

    async fn feed_windows(&mut self, mut chunk: Bytes) -> Result<(), UploadError> {
        let remaining = self.size - (self.sent - chunk.len() as u64);
        let Mode::Windowed {
            backend,
            body,
            handle,
            window,
            pacer,
        } = &mut self.mode
        else {
            unreachable!("called for the windowed mode")
        };
        let handle = *handle;
        let mut offset_into_rest = 0u64;
        while !chunk.is_empty() {
            if window.is_none() {
                let open = body
                    .take()
                    .expect("no window in flight, so the body is here");
                if open.acknowledged() > 0 {
                    // Let a call that queued for the device during the last window run first.
                    backend.let_queued_calls_in().await;
                }
                let len = pacer.next_window(remaining - offset_into_rest);
                let (tx, rx) = mpsc::channel(0);
                let data: UploadStream<'static> = Box::pin(rx);
                *window = Some(Window {
                    transfer: InFlight {
                        future: open.write_window(len, data),
                        tx,
                    },
                    len,
                    left: u64::from(len),
                    started: Instant::now(),
                });
            }
            let w = window.as_mut().expect("opened above");
            let take = (chunk.len() as u64).min(w.left) as usize;
            let part = chunk.split_to(take);
            offset_into_rest += take as u64;
            if let Err((returned, result)) = w.transfer.feed(Ok(part)).await {
                // The window ended before it took its data: a device error.
                *window = None;
                let source = result.err().unwrap_or_else(|| {
                    Error::invalid_data("the device ended a window before its data")
                });
                abandon(returned, &source).await;
                return Err(UploadError {
                    source,
                    partial: Some(handle),
                });
            }
            w.left -= take as u64;
            if w.left == 0 {
                // Close the window now, so the device is free as soon as this call returns.
                let w = window.take().expect("checked above");
                let (returned, result) = w.transfer.close().await;
                if let Err(source) = result {
                    abandon(returned, &source).await;
                    return Err(UploadError {
                        source,
                        partial: Some(handle),
                    });
                }
                pacer.record(u64::from(w.len), w.started.elapsed());
                *body = Some(returned);
            }
        }
        Ok(())
    }
}

/// Start the held-open transfer, reading its data from a channel.
fn start_held(
    backend: &Arc<dyn MtpBackend>,
    start: Option<(StorageId, Option<ObjectHandle>, NewObjectInfo)>,
) -> InFlight<HeldFuture> {
    let (storage, parent, info) = start.expect("a held-open upload starts once");
    let backend = Arc::clone(backend);
    let (tx, rx) = mpsc::channel(0);
    let future = async move {
        backend
            .upload(storage, parent, info, Box::pin(rx), None)
            .await
    }
    .boxed();
    InFlight { future, tx }
}

/// End a stopped windowed upload: cut the object back to what the device acknowledged and close
/// the edit, unless the session is gone (then the next operation on it does). The caller gets
/// the handle either way, so the object is never deleted for it.
async fn abandon(mut body: Box<dyn UploadBody>, source: &Error) {
    body.keep_on_drop();
    let session_gone = matches!(
        source,
        Error::Timeout | Error::Disconnected | Error::DeviceReset
    );
    if !session_gone {
        if let Err(e) = body.abandon().await {
            diag_debug!("upload: ending the stopped upload's edit failed: {:?}", e);
        }
    }
}

/// Upload a stream: [`FileUpload`] fed from `data`, with the progress callback told about each
/// chunk as it's read, and a `Break` stopping the upload as a cancel.
pub(crate) async fn upload_stream<'a>(
    backend: Arc<dyn MtpBackend>,
    storage: StorageId,
    parent: Option<ObjectHandle>,
    info: NewObjectInfo,
    mut data: UploadStream<'a>,
    mut progress: Option<ProgressFn<'a>>,
    pacing: UploadPacing,
) -> Result<ObjectHandle, UploadError> {
    let total = info.size;
    let mut upload = FileUpload::start(backend, storage, parent, info, pacing).await?;
    let mut read = 0u64;
    while let Some(chunk) = data.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => {
                let source = Error::Io {
                    message: e.to_string(),
                };
                return Err(upload.fail_with(e, source).await);
            }
        };
        if chunk.is_empty() {
            continue;
        }
        read += chunk.len() as u64;
        if let Some(cb) = progress.as_mut() {
            let p = Progress {
                bytes_transferred: read,
                total_bytes: Some(total),
            };
            if let ControlFlow::Break(()) = cb(p) {
                let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled");
                return Err(upload.fail_with(interrupted, Error::Cancelled).await);
            }
        }
        upload.write_chunk(chunk).await?;
    }
    upload.finish().await
}
