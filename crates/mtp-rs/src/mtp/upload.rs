//! Paced uploads: a file written as a series of windows that free the device between them.
//!
//! The policy lives here, backend-neutral, the way paced downloads live in [`super::stream`]:
//! window sizes come from the same `Pacer`, and the window loop, the yield between windows, and
//! what a stop does are decided once for every backend. A backend only provides
//! [`MtpBackend::begin_windowed_upload`] and the [`UploadBody`] it returns (today the USB backend,
//! through Android's edit operations). Why, and the hardware numbers: AGENTS.md § Paced uploads
//! and `docs/specs/paced-uploads.md`.

use crate::mtp::backend::{MtpBackend, ProgressFn, UploadBody, UploadStream};
use crate::mtp::stream::{Pacer, YieldNow};
use crate::mtp::{
    DownloadPacing, Error, NewObjectInfo, ObjectHandle, Progress, StorageId, UploadError,
};
use bytes::Bytes;
use futures::StreamExt;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

/// How long each window of [`UploadPacing::default()`] takes at most, about: 2 seconds.
pub const DEFAULT_UPLOAD_WINDOW_TIME: Duration = Duration::from_secs(2);

/// How an upload spreads a file over device transactions.
///
/// The device serves one operation at a time. One held-open `SendObject` holds it for the whole
/// file, so a folder listing from another task waits for the rest of the upload. On a device that
/// supports it ([`Capabilities::supports_windowed_upload`](crate::Capabilities::supports_windowed_upload),
/// which Android phones list), the file goes out as a series of windows instead, and the device is
/// free between them: another call waits for one window at most.
///
/// Each window costs about 1–3 ms on top of its data (Galaxy S23 Ultra, Pixel 9 Pro XL), so
/// windows of about two seconds cost well under 1%.
///
/// Devices without the operations, and the Windows WPD backend, always upload in one piece, and
/// [`MtpDeviceBuilder::use_windowed_uploads(false)`](crate::MtpDeviceBuilder::use_windowed_uploads)
/// turns windows off for a whole device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UploadPacing {
    /// One `SendObject` for the whole file, holding the device until it ends.
    HeldOpen,
    /// Windows sized from the measured speed to take about this long, so another call waits at
    /// most about this long. The first window is 8 MiB; later ones are never smaller than 1 MiB.
    /// The default, at [`DEFAULT_UPLOAD_WINDOW_TIME`].
    MaxWindowTime(Duration),
    /// Windows of a fixed size in bytes (0 counts as 1).
    FixedWindow(u32),
}

impl Default for UploadPacing {
    fn default() -> Self {
        UploadPacing::MaxWindowTime(DEFAULT_UPLOAD_WINDOW_TIME)
    }
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

/// Upload `data` as a new object, in windows where pacing and the device allow it, else in one
/// held-open transfer through [`MtpBackend::upload`].
pub(crate) async fn upload<'a>(
    backend: &dyn MtpBackend,
    storage: StorageId,
    parent: Option<ObjectHandle>,
    info: NewObjectInfo,
    data: UploadStream<'a>,
    progress: Option<ProgressFn<'a>>,
    pacing: UploadPacing,
) -> Result<ObjectHandle, UploadError> {
    // An empty file has no data to window: the held-open path is the same two operations.
    if pacing == UploadPacing::HeldOpen || info.size == 0 || !backend.can_upload_in_windows() {
        return backend.upload(storage, parent, info, data, progress).await;
    }
    let size = info.size;
    let mut body = backend
        .begin_windowed_upload(storage, parent, &info)
        .await?;
    let mut source = Source {
        inner: data,
        leftover: None,
        progress,
        read: 0,
        total: size,
    };
    let mut pacer = Pacer::new(pacing.sizing());
    while body.acknowledged() < size {
        if body.acknowledged() > 0 {
            // Let a call that queued for the device during the last window run first.
            YieldNow::default().await;
        }
        let len = pacer.next_window(size - body.acknowledged());
        let started = Instant::now();
        // The window's time counts the source's time too (reading the file): another call waits
        // for both, and that wait is what the pacing bounds.
        let written = body.write_window(len, window(&mut source, len)).await;
        if let Err(e) = written {
            return Err(stop(body, e).await);
        }
        pacer.record(u64::from(len), started.elapsed());
    }
    if let Err(e) = source.expect_end().await {
        return Err(stop(body, e).await);
    }
    let handle = body.handle();
    match body.finish().await {
        Ok(()) => Ok(handle),
        Err(source) => Err(UploadError {
            source,
            partial: Some(handle),
        }),
    }
}

/// End a stopped upload: cut the object back to what the device acknowledged and close the
/// edit, unless the session is gone (then the next operation on it does, if there is one).
/// The object stays: the caller gets its handle, to delete it or keep the prefix.
async fn stop(mut body: Box<dyn UploadBody>, source: Error) -> UploadError {
    let session_gone = matches!(
        source,
        Error::Timeout | Error::Disconnected | Error::DeviceReset
    );
    if !session_gone {
        if let Err(e) = body.abandon().await {
            diag_debug!(
                "paced upload: ending the stopped upload's edit failed: {:?}",
                e
            );
        }
    }
    UploadError {
        source,
        partial: Some(body.handle()),
    }
}

/// The caller's data, reported to its progress callback as it's read, and cut at window ends.
struct Source<'a> {
    inner: UploadStream<'a>,
    /// What's left of a chunk that went past the last window's end.
    leftover: Option<Bytes>,
    progress: Option<ProgressFn<'a>>,
    /// Bytes read from `inner` so far.
    read: u64,
    total: u64,
}

impl Source<'_> {
    /// The next chunk, at most `max` bytes. `None` when the caller's stream ended.
    async fn next(&mut self, max: u64) -> Option<std::io::Result<Bytes>> {
        let mut chunk = match self.leftover.take() {
            Some(chunk) => chunk,
            None => loop {
                match self.inner.next().await? {
                    Ok(chunk) if chunk.is_empty() => continue,
                    Ok(chunk) => {
                        self.read += chunk.len() as u64;
                        if let Err(e) = self.report() {
                            return Some(Err(e));
                        }
                        break chunk;
                    }
                    Err(e) => return Some(Err(e)),
                }
            },
        };
        if chunk.len() as u64 > max {
            self.leftover = Some(chunk.split_off(max as usize));
        }
        Some(Ok(chunk))
    }

    /// Tell the progress callback, the same way a held-open upload does: per chunk read, and a
    /// `Break` stops the upload as a cancel.
    fn report(&mut self) -> std::io::Result<()> {
        if let Some(cb) = self.progress.as_mut() {
            let progress = Progress {
                bytes_transferred: self.read,
                total_bytes: Some(self.total),
            };
            if let ControlFlow::Break(()) = cb(progress) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "cancelled",
                ));
            }
        }
        Ok(())
    }

    /// After the last window: the stream has to be over, or the file was bigger than declared.
    async fn expect_end(&mut self) -> Result<(), Error> {
        match self.next(u64::MAX).await {
            None => Ok(()),
            Some(Ok(_)) => Err(Error::invalid_data(format!(
                "the upload's data is longer than the {} bytes its NewObjectInfo declared",
                self.total
            ))),
            Some(Err(e)) => Err(Error::Io {
                message: e.to_string(),
            }),
        }
    }
}

/// The next `len` bytes of `source`, as one window's data. Ends with an error if the source ends
/// first, so the window can't go out short and the stop path takes over.
fn window<'s>(source: &'s mut Source<'_>, len: u32) -> UploadStream<'s> {
    Box::pin(futures::stream::unfold(
        (source, u64::from(len)),
        |(source, left)| async move {
            if left == 0 {
                return None;
            }
            match source.next(left).await {
                Some(Ok(chunk)) => {
                    let n = chunk.len() as u64;
                    Some((Ok(chunk), (source, left - n)))
                }
                Some(Err(e)) => Some((Err(e), (source, 0))),
                None => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                        "the upload's data ended at {} bytes, short of the {} its NewObjectInfo \
                         declared",
                        source.read, source.total
                    ),
                    )),
                    (source, 0),
                )),
            }
        },
    ))
}
