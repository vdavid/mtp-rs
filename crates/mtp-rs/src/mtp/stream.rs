//! Streaming download/upload support (backend-neutral façade types).

use crate::mtp::backend::{ByteRange, DownloadBody, MtpBackend};
use crate::mtp::{DateTime, Error, ObjectHandle, ObjectInfo};
use bytes::Bytes;
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// Progress information for transfers.
#[derive(Debug, Clone)]
#[non_exhaustive]
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

/// How long [`DownloadPacing::default()`] lets a cancel, or another call waiting for the device,
/// wait at most: 2 seconds.
pub const DEFAULT_MAX_CANCEL_DELAY: Duration = Duration::from_secs(2);

/// The first window of an adaptive download, before any speed is known.
const FIRST_WINDOW: u64 = 8 * MIB;
/// The smallest adaptive window. Each window costs ~15–20 ms on a Pixel 9 Pro XL on top of its
/// data, so much smaller windows spend more time on that than on the data.
const MIN_WINDOW: u64 = MIB;
const MIB: u64 = 1024 * 1024;

/// How a [`FileDownload`] spreads a file over device transactions.
///
/// The device serves one operation at a time. A single held-open transfer is the fastest way to
/// read a file, but it holds the device until it ends, and on Android a cancel can't cut it
/// short: it reads the rest of the file (see [`FileDownload::cancel`]). Reading the file as a
/// series of windows frees the device between windows, so a cancel only finishes the current
/// window, and other calls (a folder listing) get their turn in between.
///
/// Each window costs a little on top of its data (~15–20 ms on a Pixel 9 Pro XL), so windows
/// sized from the measured speed to take about the target time cost ~1% at any link speed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DownloadPacing {
    /// One transfer for the whole range. Fastest, but it holds the device until it ends, and a
    /// cancel on Android costs the rest of the range.
    HeldOpen,
    /// Windows sized from the measured speed so a cancel, or another call waiting for the device,
    /// waits at most about this long. The first window is 8 MiB; later ones are never smaller
    /// than 1 MiB. The default, at [`DEFAULT_MAX_CANCEL_DELAY`].
    MaxCancelDelay(Duration),
    /// Windows of a fixed size in bytes (0 counts as 1).
    FixedWindow(u32),
}

impl Default for DownloadPacing {
    fn default() -> Self {
        DownloadPacing::MaxCancelDelay(DEFAULT_MAX_CANCEL_DELAY)
    }
}

/// Picks window sizes for a paced download, and for a paced upload (see `mtp::upload`).
#[derive(Debug)]
pub(crate) struct Pacer {
    pacing: DownloadPacing,
    /// Smoothed bytes per second of device time, once a window has finished.
    rate: Option<f64>,
}

impl Pacer {
    pub(crate) fn new(pacing: DownloadPacing) -> Self {
        Self { pacing, rate: None }
    }

    fn held_open(&self) -> bool {
        matches!(self.pacing, DownloadPacing::HeldOpen)
    }

    /// The size of the next window, given the bytes still to read.
    pub(crate) fn next_window(&self, remaining: u64) -> u32 {
        let want = match self.pacing {
            DownloadPacing::FixedWindow(size) => u64::from(size.max(1)),
            DownloadPacing::MaxCancelDelay(target) => match self.rate {
                None => FIRST_WINDOW,
                Some(rate) => {
                    let bytes = (rate * target.as_secs_f64()) as u64;
                    (bytes / MIB * MIB).max(MIN_WINDOW)
                }
            },
            DownloadPacing::HeldOpen => remaining,
        };
        u32::try_from(want.min(remaining)).unwrap_or(u32::MAX)
    }

    /// Learn from a finished window: `bytes` read in `device_time`.
    pub(crate) fn record(&mut self, bytes: u64, device_time: Duration) {
        if bytes == 0 || device_time.is_zero() {
            return;
        }
        let rate = bytes as f64 / device_time.as_secs_f64();
        // Half the weight on the newest window: it follows a real change in a few windows and
        // doesn't jump on one slow one.
        self.rate = Some(self.rate.map_or(rate, |old| (old + rate) / 2.0));
    }
}

/// One read in flight: a held-open transfer or one window.
struct ActiveRead {
    body: Box<dyn DownloadBody>,
    held_open: bool,
    /// Bytes asked for.
    requested: u64,
    /// Bytes received so far.
    received: u64,
    /// Time spent waiting on the device for this read's chunks, not counting the time to start
    /// the read or the consumer's time between chunks. The body's own measurement wins when it
    /// has one ([`ActiveRead::device_time`]).
    device_time: Duration,
}

impl ActiveRead {
    /// How long the device took for this read: the body's own measurement when it reads ahead
    /// (WPD), else the time waited for its chunks.
    fn device_time(&self) -> Duration {
        self.body.device_time().unwrap_or(self.device_time)
    }
}

/// A file download in progress, streamed from the device.
///
/// Created by [`Storage::download`](crate::mtp::Storage::download) or
/// [`Storage::download_with_pacing`](crate::mtp::Storage::download_with_pacing). Chunks arrive
/// as the device sends them, so memory stays at about one 64 KiB read whatever the file's size.
/// The [`DownloadPacing`] decides how the file is spread over device transactions; by default
/// it's read in windows that each take about two seconds, and the device is free between them.
///
/// # Stopping early
///
/// Call [`cancel()`](Self::cancel) to stop and free the device right away. Dropping the download
/// is safe too: between windows it holds nothing, and mid-window the next operation on the device
/// clears what's left of the window first.
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
#[must_use = "a FileDownload does nothing until you read it with next_chunk()"]
pub struct FileDownload {
    backend: Arc<dyn MtpBackend>,
    handle: ObjectHandle,
    size: u64,
    /// Where the requested range starts.
    start: u64,
    /// Where the requested range ends (exclusive).
    end: u64,
    /// The next byte to read.
    offset: u64,
    modified: Option<DateTime>,
    created: Option<DateTime>,
    pacer: Pacer,
    current: Option<ActiveRead>,
    /// Whether any read has started, so the yield between windows skips the first.
    started: bool,
    done: bool,
}

impl FileDownload {
    /// Set up a download of `range` of the object described by `info`. A held-open download
    /// starts its transfer here, so its errors surface right away; a windowed one starts its
    /// first window on the first [`next_chunk`](Self::next_chunk).
    pub(crate) async fn start(
        backend: Arc<dyn MtpBackend>,
        handle: ObjectHandle,
        info: &ObjectInfo,
        range: ByteRange,
        pacing: DownloadPacing,
    ) -> Result<Self, Error> {
        let size = info.size;
        let start = range.offset();
        if start > size {
            return Err(Error::invalid_data(format!(
                "download offset {start} is past the object size {size}"
            )));
        }
        let end = match range {
            ByteRange::Range { len, .. } => start.saturating_add(len).min(size),
            _ => size,
        };
        let mut download = Self {
            backend,
            handle,
            size,
            start,
            end,
            offset: start,
            modified: info.modified,
            created: info.created,
            pacer: Pacer::new(pacing),
            current: None,
            started: false,
            done: false,
        };
        // A whole file that partial reads can't cover to the end (no partial-read op, or only the
        // 32-bit one and a file past 4 GiB) is one `GetObject`, which works at any size. Deciding
        // up front matters: windows would otherwise run until one has to start past 4 GiB, then fail.
        let whole_file = start == 0 && end == size;
        if whole_file && end > 0 && !download.backend.can_read_partially_at(end - 1) {
            download.pacer = Pacer::new(DownloadPacing::HeldOpen);
        }
        // A held-open read starts here, so a refusal (a ranged read on a device with no partial
        // reads) surfaces at the call, not at the first chunk. Windows start lazily.
        let held_open =
            download.pacer.held_open() || !download.backend.can_read_partially_at(start);
        if held_open && start < end {
            download.start_next_read().await?;
        }
        Ok(download)
    }

    /// Total file size in bytes (always the whole file, even for a ranged download).
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Bytes received so far in this download.
    #[must_use]
    pub fn bytes_received(&self) -> u64 {
        self.offset - self.start
    }

    /// File offset of the next byte to arrive. Resume from here with
    /// [`ByteRange::From`] after a cancel.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// How far through the file this download is, as a fraction (0.0 to 1.0). Anchored to the
    /// whole file, so a resumed download picks up where the last one left off.
    #[must_use]
    pub fn progress(&self) -> f64 {
        if self.size == 0 {
            1.0
        } else {
            self.offset as f64 / self.size as f64
        }
    }

    /// The object's modification time, from the `ObjectInfo` fetched to start the download, so
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

    /// Whether a transfer is in flight right now, holding the device's one session.
    ///
    /// While it's `false` (between windows, and once the download is over), other calls on the
    /// device run right away. A consumer with its own device lock can hold it while this is `true`
    /// and let other work in when it's `false`. Don't hold a lock that blocks this download while
    /// waiting for the device: a window in flight only gives the device back once it's read.
    #[must_use]
    pub fn holds_session(&self) -> bool {
        self.current.is_some()
    }

    /// Stop the download and give the device back.
    ///
    /// Between windows this sends nothing. Mid-window (or mid-transfer, for
    /// [`DownloadPacing::HeldOpen`]) the USB backend stops the transfer and drains what's left
    /// of it, leaving the session clean for the next operation. `idle_timeout` controls how long
    /// to wait during that drain; [`DEFAULT_CANCEL_TIMEOUT`] is the usual choice.
    ///
    /// **On Android this reads the rest of the current transfer**: Android's MTP server can die
    /// until a replug when it gets the Still Image Class cancel request mid-transfer, so it never
    /// gets one. With the default pacing that's at most the current window, about two seconds; with
    /// [`DownloadPacing::HeldOpen`] it's the rest of the file.
    pub async fn cancel(&mut self, idle_timeout: Duration) -> Result<(), Error> {
        self.done = true;
        match self.current.take() {
            Some(mut read) => read.body.cancel(idle_timeout).await,
            None => Ok(()),
        }
    }

    /// Get the next chunk of data. Returns `None` when the download is complete.
    pub async fn next_chunk(&mut self) -> Option<Result<Bytes, Error>> {
        if self.done {
            return None;
        }
        loop {
            if let Some(read) = self.current.as_mut() {
                let waited = Instant::now();
                let item = read.body.next_chunk().await;
                read.device_time += waited.elapsed();
                match item {
                    Some(Ok(chunk)) => {
                        read.received += chunk.len() as u64;
                        let (received, requested, held_open) =
                            (read.received, read.requested, read.held_open);
                        self.offset += chunk.len() as u64;
                        if !held_open && received > requested {
                            let read_start = self.offset - received;
                            return Some(Err(self.fail(Error::invalid_data(format!(
                                "device sent {received} bytes for a {requested}-byte read at \
                                 offset {read_start}"
                            )))));
                        }
                        if !held_open && received == requested {
                            // Read the closing response now, so the device is free as soon as
                            // this call returns the window's last bytes.
                            if let Err(e) = self.close_full_window().await {
                                return Some(Err(self.fail(e)));
                            }
                        }
                        return Some(Ok(chunk));
                    }
                    Some(Err(e)) => return Some(Err(self.fail(e))),
                    None => {
                        let read = self.current.take().expect("matched Some above");
                        if read.received == 0 && self.offset < self.end {
                            return Some(Err(self.fail(Error::invalid_data(format!(
                                "device returned no data at offset {} of {} (expected up to {}); \
                                 treating it as a stall, not the end of the file",
                                self.offset, self.size, read.requested
                            )))));
                        }
                        if !read.held_open {
                            self.pacer.record(read.received, read.device_time());
                        }
                        // A short read is legal: the next read picks up at `offset`.
                    }
                }
            }
            if self.offset >= self.end {
                self.done = true;
                return None;
            }
            if let Err(e) = self.start_next_read().await {
                return Some(Err(self.fail(e)));
            }
        }
    }

    /// Finish a window whose requested bytes have all arrived: its next item has to be the end.
    async fn close_full_window(&mut self) -> Result<(), Error> {
        let mut read = self.current.take().expect("a window is in flight");
        let waited = Instant::now();
        let item = read.body.next_chunk().await;
        read.device_time += waited.elapsed();
        match item {
            None => {
                self.pacer.record(read.received, read.device_time());
                Ok(())
            }
            Some(Ok(extra)) => Err(Error::invalid_data(format!(
                "device sent {} bytes more than the {} asked for at offset {}",
                extra.len(),
                read.requested,
                self.offset - read.received
            ))),
            Some(Err(e)) => Err(e),
        }
    }

    /// Start the next read: a window, or a held-open transfer for the rest of the range when the
    /// pacing asks for one or the device can't read partially from here.
    ///
    /// The read's `device_time` starts at zero: the time to start it (the yield, and on WPD a
    /// stream reopen and seek, which re-reads the prefix on a forward-only stream) isn't the
    /// transfer rate, and counting it shrank every window after an interleaved call (#61).
    async fn start_next_read(&mut self) -> Result<(), Error> {
        let remaining = self.end - self.offset;
        let windowed = !self.pacer.held_open() && self.backend.can_read_partially_at(self.offset);
        let read = if windowed {
            if self.started {
                // Let a call that queued for the device during the last window run first.
                YieldNow::default().await;
            }
            let len = self.pacer.next_window(remaining);
            let body = self
                .backend
                .download_window(self.handle, self.offset, len)
                .await?;
            ActiveRead {
                body,
                held_open: false,
                requested: u64::from(len),
                received: 0,
                device_time: Duration::ZERO,
            }
        } else {
            let range = if self.offset == 0 && self.end == self.size {
                ByteRange::Full
            } else {
                ByteRange::Range {
                    offset: self.offset,
                    len: remaining,
                }
            };
            let body = self.backend.download(self.handle, range, self.size).await?;
            ActiveRead {
                body,
                held_open: true,
                requested: remaining,
                received: 0,
                device_time: Duration::ZERO,
            }
        };
        self.current = Some(read);
        self.started = true;
        Ok(())
    }

    /// End the download on an error, dropping any read in flight (its body cleans up the session).
    fn fail(&mut self, e: Error) -> Error {
        self.current = None;
        self.done = true;
        e
    }

    /// Consume the download and iterate with a progress callback.
    ///
    /// Calls `on_progress` after each chunk. Return `ControlFlow::Break(())` to cancel the download.
    pub async fn collect_with_progress<F>(mut self, mut on_progress: F) -> Result<Vec<u8>, Error>
    where
        F: FnMut(Progress) -> ControlFlow<()>,
    {
        let mut data = Vec::with_capacity((self.end - self.offset) as usize);

        while let Some(result) = self.next_chunk().await {
            let chunk = result?;
            data.extend_from_slice(&chunk);

            let progress = Progress {
                bytes_transferred: self.bytes_received(),
                total_bytes: Some(self.end - self.start),
            };

            if let ControlFlow::Break(()) = on_progress(progress) {
                self.cancel(DEFAULT_CANCEL_TIMEOUT).await?;
                return Err(Error::Cancelled);
            }
        }

        Ok(data)
    }

    /// Collect all remaining data into a `Vec<u8>`. Consumes the download.
    pub async fn collect(mut self) -> Result<Vec<u8>, Error> {
        let mut data = Vec::with_capacity((self.end - self.offset) as usize);
        while let Some(result) = self.next_chunk().await {
            data.extend_from_slice(&result?);
        }
        Ok(data)
    }
}

/// Returns `Pending` once, so other tasks the executor has ready (a call that was waiting for the
/// device) get to run before this one continues. Runtime-agnostic.
#[derive(Default)]
pub(crate) struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * MIB;

    #[test]
    fn adaptive_windows_start_at_8_mib_then_take_about_the_target_time() {
        let mut pacer = Pacer::new(DownloadPacing::MaxCancelDelay(Duration::from_secs(2)));
        assert_eq!(pacer.next_window(GIB), 8 * MIB as u32);

        // 8 MiB in 50 ms is ~168 MB/s: two seconds of that, rounded down to whole MiB.
        pacer.record(8 * MIB, Duration::from_millis(50));
        assert_eq!(pacer.next_window(GIB), 320 * MIB as u32);

        // A slow window pulls the estimate halfway down, not all the way.
        pacer.record(8 * MIB, Duration::from_millis(150));
        assert_eq!(pacer.next_window(GIB), 213 * MIB as u32);
    }

    #[test]
    fn adaptive_windows_stay_between_1_mib_and_the_partial_read_limit() {
        let mut slow = Pacer::new(DownloadPacing::MaxCancelDelay(Duration::from_secs(2)));
        slow.record(64 * 1024, Duration::from_secs(1)); // 64 KiB/s
        assert_eq!(slow.next_window(GIB), MIB as u32);

        let mut fast = Pacer::new(DownloadPacing::MaxCancelDelay(Duration::from_secs(60)));
        fast.record(GIB, Duration::from_millis(100));
        assert_eq!(fast.next_window(100 * GIB), u32::MAX);

        // Never more than what's left.
        assert_eq!(fast.next_window(1234), 1234);
    }

    #[test]
    fn a_window_with_nothing_to_learn_from_changes_nothing() {
        let mut pacer = Pacer::new(DownloadPacing::default());
        pacer.record(0, Duration::from_millis(10));
        pacer.record(MIB, Duration::ZERO);
        assert_eq!(pacer.rate, None);
    }

    #[test]
    fn fixed_and_held_open_pacing() {
        let fixed = Pacer::new(DownloadPacing::FixedWindow(4096));
        assert_eq!(fixed.next_window(GIB), 4096);
        assert_eq!(fixed.next_window(100), 100);
        assert_eq!(
            Pacer::new(DownloadPacing::FixedWindow(0)).next_window(GIB),
            1
        );
        assert!(Pacer::new(DownloadPacing::HeldOpen).held_open());
        assert!(!Pacer::new(DownloadPacing::default()).held_open());
    }

    /// A backend whose windows take `open_delay` to start (WPD reopening and seeking a stream
    /// after another call ran) and `stream_delay` to stream their bytes. Only `download_window`
    /// and `can_read_partially_at` are real.
    struct SlowStartBackend {
        open_delay: Duration,
        stream_delay: Duration,
        /// What each window's body reports as its device time (`DownloadBody::device_time`).
        reported: Option<Duration>,
        windows: std::sync::Mutex<Vec<u32>>,
        device_info: crate::mtp::DeviceInfo,
        capabilities: crate::mtp::Capabilities,
    }

    struct SlowBody {
        left: u32,
        stream_delay: Option<Duration>,
        reported: Option<Duration>,
    }

    #[async_trait::async_trait]
    impl DownloadBody for SlowBody {
        async fn next_chunk(&mut self) -> Option<Result<Bytes, Error>> {
            if self.left == 0 {
                return None;
            }
            if let Some(delay) = self.stream_delay.take() {
                std::thread::sleep(delay);
            }
            let n = self.left.min(MIB as u32);
            self.left -= n;
            Some(Ok(Bytes::from(vec![0u8; n as usize])))
        }

        async fn cancel(&mut self, _idle_timeout: Duration) -> Result<(), Error> {
            self.left = 0;
            Ok(())
        }

        fn device_time(&self) -> Option<Duration> {
            self.reported
        }
    }

    #[async_trait::async_trait]
    impl MtpBackend for SlowStartBackend {
        fn device_info(&self) -> &crate::mtp::DeviceInfo {
            &self.device_info
        }
        fn capabilities(&self) -> &crate::mtp::Capabilities {
            &self.capabilities
        }
        async fn download_window(
            &self,
            _obj: ObjectHandle,
            _offset: u64,
            len: u32,
        ) -> Result<Box<dyn DownloadBody>, Error> {
            self.windows.lock().unwrap().push(len);
            std::thread::sleep(self.open_delay);
            Ok(Box::new(SlowBody {
                left: len,
                stream_delay: Some(self.stream_delay),
                reported: self.reported,
            }))
        }
        fn can_read_partially_at(&self, _offset: u64) -> bool {
            true
        }
        async fn storages(&self) -> Result<Vec<crate::mtp::StorageInfo>, Error> {
            unimplemented!()
        }
        async fn storage_info(
            &self,
            _storage: crate::mtp::StorageId,
        ) -> Result<crate::mtp::StorageInfo, Error> {
            unimplemented!()
        }
        async fn list(
            &self,
            _storage: crate::mtp::StorageId,
            _parent: Option<ObjectHandle>,
            _cancel: Option<&crate::cancel::CancelToken>,
        ) -> Result<crate::mtp::backend::BackendListing, Error> {
            unimplemented!()
        }
        async fn object_info(&self, _obj: ObjectHandle) -> Result<ObjectInfo, Error> {
            unimplemented!()
        }
        async fn download(
            &self,
            _obj: ObjectHandle,
            _range: ByteRange,
            _size: u64,
        ) -> Result<Box<dyn DownloadBody>, Error> {
            unimplemented!()
        }
        async fn read_range(
            &self,
            _obj: ObjectHandle,
            _offset: u64,
            _len: Option<u32>,
        ) -> Result<Vec<u8>, Error> {
            unimplemented!()
        }
        async fn thumbnail(&self, _obj: ObjectHandle) -> Result<Vec<u8>, Error> {
            unimplemented!()
        }
        async fn upload(
            &self,
            _storage: crate::mtp::StorageId,
            _parent: Option<ObjectHandle>,
            _info: crate::mtp::object::NewObjectInfo,
            _data: crate::mtp::backend::UploadStream<'_>,
            _progress: Option<crate::mtp::backend::ProgressFn<'_>>,
        ) -> Result<ObjectHandle, crate::mtp::UploadError> {
            unimplemented!()
        }
        async fn create_folder(
            &self,
            _storage: crate::mtp::StorageId,
            _parent: Option<ObjectHandle>,
            _name: &str,
        ) -> Result<ObjectHandle, Error> {
            unimplemented!()
        }
        async fn delete(
            &self,
            _obj: ObjectHandle,
            _cancel: Option<&crate::cancel::CancelToken>,
        ) -> Result<(), Error> {
            unimplemented!()
        }
        async fn move_object(
            &self,
            _obj: ObjectHandle,
            _new_parent: ObjectHandle,
            _new_storage: crate::mtp::StorageId,
        ) -> Result<(), Error> {
            unimplemented!()
        }
        async fn copy_object(
            &self,
            _obj: ObjectHandle,
            _new_parent: ObjectHandle,
            _new_storage: crate::mtp::StorageId,
        ) -> Result<ObjectHandle, Error> {
            unimplemented!()
        }
        async fn rename(&self, _obj: ObjectHandle, _new_name: &str) -> Result<(), Error> {
            unimplemented!()
        }
        async fn next_event(&self) -> Result<crate::mtp::DeviceEvent, Error> {
            unimplemented!()
        }
        async fn close(&self) -> Result<(), Error> {
            unimplemented!()
        }
    }

    /// #61: on WPD a window after an interleaved call first reopens the stream (and on a
    /// forward-only stream reads the whole prefix again). That's not the transfer rate, so it
    /// must not shrink the next window: only the time spent streaming counts.
    #[test]
    fn a_slow_window_start_does_not_shrink_the_next_window() {
        let backend = Arc::new(SlowStartBackend {
            open_delay: Duration::from_millis(500),
            // 8 MiB in ~50 ms is ~168 MB/s, so a 2 s window is ~320 MiB.
            stream_delay: Duration::from_millis(50),
            reported: None,
            windows: std::sync::Mutex::new(Vec::new()),
            device_info: Default::default(),
            capabilities: Default::default(),
        });
        let info = ObjectInfo {
            size: GIB,
            ..Default::default()
        };
        futures::executor::block_on(async {
            let mut download = FileDownload::start(
                backend.clone(),
                ObjectHandle(1),
                &info,
                ByteRange::Full,
                DownloadPacing::MaxCancelDelay(Duration::from_secs(2)),
            )
            .await
            .unwrap();
            // Read the whole first window, then one chunk of the second, which starts it.
            while download.bytes_received() <= 8 * MIB {
                download.next_chunk().await.unwrap().unwrap();
            }
        });
        let windows = backend.windows.lock().unwrap().clone();
        assert_eq!(windows.len(), 2, "windows: {windows:?}");
        assert_eq!(windows[0], 8 * MIB as u32);
        // Counting the 0.5 s start would give ~29 MiB. Leave room for a slow test machine.
        assert!(
            u64::from(windows[1]) >= 64 * MIB,
            "second window {} MiB: the slow start was counted as transfer time",
            u64::from(windows[1]) / MIB
        );
    }

    /// #61: WPD's worker reads ahead into a channel while the consumer handles earlier chunks
    /// (writes them to disk, say), so the consumer's waits undercount the device's time and every
    /// window came out bigger than the last: 500 ms windows grew to 5.7 s on a Pixel under
    /// Windows. A body that measures the device's time itself is believed over those waits.
    #[test]
    fn a_body_that_times_the_device_itself_sizes_the_next_window() {
        let backend = Arc::new(SlowStartBackend {
            open_delay: Duration::ZERO,
            // The chunks were read ahead, so they arrive at once...
            stream_delay: Duration::ZERO,
            // ...but the device took 2 s for the 8 MiB: 4 MiB/s, so a 2 s window is 8 MiB.
            reported: Some(Duration::from_secs(2)),
            windows: std::sync::Mutex::new(Vec::new()),
            device_info: Default::default(),
            capabilities: Default::default(),
        });
        let info = ObjectInfo {
            size: GIB,
            ..Default::default()
        };
        futures::executor::block_on(async {
            let mut download = FileDownload::start(
                backend.clone(),
                ObjectHandle(1),
                &info,
                ByteRange::Full,
                DownloadPacing::MaxCancelDelay(Duration::from_secs(2)),
            )
            .await
            .unwrap();
            while download.bytes_received() <= 8 * MIB {
                download.next_chunk().await.unwrap().unwrap();
            }
        });
        let windows = backend.windows.lock().unwrap().clone();
        assert_eq!(windows, vec![8 * MIB as u32, 8 * MIB as u32]);
    }

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
