//! Streaming download/upload support (backend-neutral façade types).

use crate::mtp::backend::{DownloadBody, MtpBackend};
use crate::mtp::{DateTime, Error, ObjectHandle, ObjectInfo};
use bytes::Bytes;
use std::ops::ControlFlow;
use std::sync::Arc;

/// Progress information for transfers.
#[derive(Debug, Clone)]
pub struct Progress {
    /// Bytes transferred so far.
    pub bytes_transferred: u64,
    /// Total bytes (if known).
    pub total_bytes: Option<u64>,
}

impl Progress {
    /// Progress as a percentage (0.0 to 100.0).
    #[must_use]
    pub fn percent(&self) -> f64 {
        self.fraction() * 100.0
    }

    /// Progress as a fraction (0.0 to 1.0).
    #[must_use]
    pub fn fraction(&self) -> f64 {
        self.total_bytes.map_or(1.0, |total| {
            if total == 0 {
                1.0
            } else {
                self.bytes_transferred as f64 / total as f64
            }
        })
    }
}

/// Default idle timeout for cancel drain operations.
///
/// After sending the cancel request, this is how long to wait for additional data on each pipe
/// before assuming it's clear. Matches the 300ms timeout used by libmtp, which mirrors Windows
/// behavior.
pub const DEFAULT_CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// A file download in progress, streamed from the device.
///
/// Wraps the active backend's download body and tracks progress. Data is streamed directly from the
/// device as chunks arrive, without buffering the entire file in memory.
///
/// # Important
///
/// On the USB backend the session is held while this download is active. You must either consume
/// the entire download or call [`cancel()`](Self::cancel) before dropping it; cancelling drains the
/// pipe and frees the session.
///
/// # Example
///
/// ```rust,no_run
/// use mtp_rs::mtp::MtpDevice;
/// use mtp_rs::{ByteRange, ObjectHandle};
/// use tokio::io::AsyncWriteExt;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let device = MtpDevice::open_first().await?;
/// # let storages = device.storages().await?;
/// # let storage = &storages[0];
/// # let handle = ObjectHandle(1);
/// let mut download = storage.download(handle, ByteRange::Full).await?;
/// println!("Downloading {} bytes...", download.size());
///
/// # let mut file = tokio::fs::File::create("output.bin").await?;
/// while let Some(chunk) = download.next_chunk().await {
///     let bytes = chunk?;
///     file.write_all(&bytes).await?;
///     println!("Progress: {:.1}%", download.progress() * 100.0);
/// }
/// # Ok(())
/// # }
/// ```
#[must_use = "dropping a FileDownload mid-transfer may corrupt the USB session; \
               consume it fully or call cancel()"]
pub struct FileDownload {
    size: u64,
    bytes_received: u64,
    body: Box<dyn DownloadBody>,
}

impl FileDownload {
    /// Create a new FileDownload wrapping a backend download body.
    pub(crate) fn new(size: u64, body: Box<dyn DownloadBody>) -> Self {
        Self {
            size,
            bytes_received: 0,
            body,
        }
    }

    /// Total file size in bytes (always the whole file, even for a ranged download).
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Bytes received so far in this stream.
    #[must_use]
    pub fn bytes_received(&self) -> u64 {
        self.bytes_received
    }

    /// Progress as a fraction (0.0 to 1.0).
    #[must_use]
    pub fn progress(&self) -> f64 {
        if self.size == 0 {
            1.0
        } else {
            self.bytes_received as f64 / self.size as f64
        }
    }

    /// Cancel the in-progress download.
    ///
    /// On the USB backend this uses the Still Image Class cancel mechanism to stop the transfer and
    /// drain remaining data, leaving the session clean for the next operation. The `idle_timeout`
    /// controls how long to wait during the pipe drain. If the download is already complete, this
    /// is a no-op.
    pub async fn cancel(&mut self, idle_timeout: std::time::Duration) -> Result<(), Error> {
        self.body.cancel(idle_timeout).await
    }

    /// Get the next chunk of data. Returns `None` when the download is complete.
    pub async fn next_chunk(&mut self) -> Option<Result<Bytes, Error>> {
        match self.body.next_chunk().await {
            Some(Ok(bytes)) => {
                self.bytes_received += bytes.len() as u64;
                Some(Ok(bytes))
            }
            other => other,
        }
    }

    /// Consume the download and iterate with a progress callback.
    ///
    /// Calls `on_progress` after each chunk. Return `ControlFlow::Break(())` to cancel the download.
    pub async fn collect_with_progress<F>(mut self, mut on_progress: F) -> Result<Vec<u8>, Error>
    where
        F: FnMut(Progress) -> ControlFlow<()>,
    {
        let mut data = Vec::with_capacity(self.size as usize);

        while let Some(result) = self.next_chunk().await {
            let chunk = result?;
            data.extend_from_slice(&chunk);

            let progress = Progress {
                bytes_transferred: self.bytes_received,
                total_bytes: Some(self.size),
            };

            if let ControlFlow::Break(()) = on_progress(progress) {
                self.body.cancel(DEFAULT_CANCEL_TIMEOUT).await?;
                return Err(Error::Cancelled);
            }
        }

        Ok(data)
    }

    /// Collect all remaining data into a `Vec<u8>`. Consumes the download.
    pub async fn collect(mut self) -> Result<Vec<u8>, Error> {
        let mut data = Vec::with_capacity(self.size as usize);
        while let Some(result) = self.next_chunk().await {
            data.extend_from_slice(&result?);
        }
        Ok(data)
    }
}

/// Default window size for [`Storage::download_windowed`](crate::mtp::Storage::download_windowed):
/// 8 MiB.
///
/// Each window is one bounded transaction that **releases** the one-per-device session the moment it
/// returns. On a Pixel 9 Pro XL an 8 MiB window completes in roughly 80 ms: small enough that a
/// concurrent folder listing or navigation slips in between windows at its natural cost, yet large
/// enough to keep throughput high.
///
/// This is a documented suggestion, not a baked-in policy: pass your own `window_size` to
/// [`download_windowed`](crate::mtp::Storage::download_windowed) to tune the
/// responsiveness/throughput tradeoff for your device and workload.
pub const DEFAULT_DOWNLOAD_WINDOW: u32 = 8 * 1024 * 1024;

/// A large-file reader that fetches the file as a sequence of bounded windows, **releasing the
/// session between every window**.
///
/// A streaming [`FileDownload`] holds the device's single session open for the *entire* file, so
/// while a big download is in flight no other operation (a folder listing, navigation) can touch
/// the device until the read finishes or is cancelled. `WindowedDownload` solves that: each
/// [`next_window()`](Self::next_window) is a single, short bounded read that completes and frees the
/// session, so a listing issued between two windows just works.
///
/// # The consumer owns the policy
///
/// `WindowedDownload` owns the *bookkeeping* (total size, current offset, window sizing, EOF
/// detection). It deliberately owns **no** policy: there's no pause, debounce, rate-limit, or
/// priority gate baked in. Whatever a consumer wants to do "while the session is free" it does
/// *between* `next_window()` calls.
///
/// # Stopping early needs no `cancel()`
///
/// Unlike [`FileDownload`], a `WindowedDownload` holds nothing between windows. To stop early, just
/// stop calling `next_window()` and drop it.
pub struct WindowedDownload {
    backend: Arc<dyn MtpBackend>,
    handle: ObjectHandle,
    /// Full object size, cached at construction so progress/ETA stays anchored.
    total_size: u64,
    /// Byte offset of the next window to fetch.
    offset: u64,
    /// Max bytes requested per window.
    window_size: u32,
    /// From the `ObjectInfo` fetched at construction.
    modified: Option<DateTime>,
    /// From the `ObjectInfo` fetched at construction.
    created: Option<DateTime>,
}

impl WindowedDownload {
    /// Create a windowed download starting at `start_offset`.
    ///
    /// `window_size` is clamped to at least 1 byte: a zero window can't make progress. Size and
    /// dates come from `info`.
    pub(crate) fn new(
        backend: Arc<dyn MtpBackend>,
        handle: ObjectHandle,
        info: &ObjectInfo,
        start_offset: u64,
        window_size: u32,
    ) -> Self {
        Self {
            backend,
            handle,
            total_size: info.size,
            offset: start_offset,
            window_size: window_size.max(1),
            modified: info.modified,
            created: info.created,
        }
    }

    /// Full object size in bytes (always the whole file, even for a windowed download started at a
    /// non-zero offset, so progress/ETA stays anchored to the complete file).
    #[must_use]
    pub fn size(&self) -> u64 {
        self.total_size
    }

    /// The object's modification time from the `ObjectInfo` fetched to start the download, so
    /// keeping a copy's date costs no extra round trip. `None` when the device reported none (or
    /// one that doesn't parse). Usually zoneless: see [`DateTime`] for resolving it.
    #[must_use]
    pub fn modified(&self) -> Option<DateTime> {
        self.modified
    }

    /// The object's creation time, from the same `ObjectInfo` as [`modified`](Self::modified).
    /// Many devices leave it empty.
    #[must_use]
    pub fn created(&self) -> Option<DateTime> {
        self.created
    }

    /// Byte offset of the next window to be read.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read the next window via one bounded read, **releasing the session on return**.
    ///
    /// Returns:
    /// - `Some(Ok(bytes))`: the next window, clamped to the bytes remaining. A device may legally
    ///   return *fewer* bytes than requested mid-file; the offset advances by what actually came
    ///   back.
    /// - `None`: clean end of file (also the first result for an empty file, or for a download
    ///   started at `offset == size`). No transaction is issued in that case.
    /// - `Some(Err(_))`: a transfer error, **or** a device stall (a 0-byte read while bytes still
    ///   remain), reported as [`Error::InvalidData`] rather than silently treated as EOF.
    pub async fn next_window(&mut self) -> Option<Result<Vec<u8>, Error>> {
        // EOF: nothing left. Covers the empty-file and offset==size cases too, and issues no
        // transaction.
        if self.offset >= self.total_size {
            return None;
        }

        // Clamp the request to the bytes that remain and to the configured window size.
        let remaining = self.total_size - self.offset;
        let want = u32::try_from(remaining.min(u64::from(self.window_size))).unwrap_or(u32::MAX);

        match self
            .backend
            .read_range(self.handle, self.offset, Some(want))
            .await
        {
            Ok(bytes) => {
                if bytes.is_empty() {
                    // We asked for >0 bytes but the device returned none: a stall, not EOF.
                    return Some(Err(Error::invalid_data(format!(
                        "device returned 0 bytes at offset {} of {} (expected up to {want}); \
                         treating as a stall rather than end-of-file",
                        self.offset, self.total_size
                    ))));
                }
                // A short non-zero read is legal: advance by what actually came back.
                self.offset += bytes.len() as u64;
                Some(Ok(bytes))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_calculations() {
        let cases = [
            (50, Some(100), 50.0, 0.5),
            (100, Some(100), 100.0, 1.0),
            (25, Some(100), 25.0, 0.25),
            (0, Some(0), 100.0, 1.0), // Empty file
            (50, None, 100.0, 1.0),   // Unknown total defaults to complete
        ];
        for (transferred, total, expected_pct, expected_frac) in cases {
            let p = Progress {
                bytes_transferred: transferred,
                total_bytes: total,
            };
            assert_eq!(
                p.percent(),
                expected_pct,
                "percent failed for {transferred}/{total:?}"
            );
            assert_eq!(
                p.fraction(),
                expected_frac,
                "fraction failed for {transferred}/{total:?}"
            );
        }

        // Large numbers
        let large = Progress {
            bytes_transferred: u64::MAX / 2,
            total_bytes: Some(u64::MAX),
        };
        let frac = large.fraction();
        assert!(frac > 0.49 && frac < 0.51);
    }
}
