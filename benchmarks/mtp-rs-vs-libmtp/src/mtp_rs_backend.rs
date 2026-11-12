//! Benchmark backend using the pure-Rust mtp-rs crate.

use mtp_rs::mtp::MtpDevice;
use mtp_rs::mtp::NewObjectInfo;
use mtp_rs::DownloadPacing;
use mtp_rs::ObjectHandle;
use mtp_rs::StorageId;

/// Backend wrapping mtp-rs for benchmark comparison.
pub struct MtpRsBackend {
    device: MtpDevice,
    storage_id: StorageId,
    /// Handle to the temporary benchmark folder inside Download.
    bench_folder_handle: ObjectHandle,
    /// Cached device description string.
    description: String,
}

impl MtpRsBackend {
    /// Connect to the first MTP device, get its first storage,
    /// and create a temporary benchmark folder (e.g. "mtp-bench-tmp") in the Download folder.
    /// The Download folder is the safest target on Android.
    pub async fn connect(serial: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        Self::with_device(open_device(serial).await?).await
    }

    /// [`connect`](Self::connect) on an already open device.
    pub async fn with_device(device: MtpDevice) -> Result<Self, Box<dyn std::error::Error>> {
        let info = device.device_info();
        let description = format!("{} {}", info.manufacturer, info.model);

        let storages = device.storages().await?;
        let storage = storages
            .into_iter()
            .next()
            .ok_or("No storage found on device")?;
        let storage_id = storage.id();

        // Find the "Download" or "Downloads" folder in the root of the first storage.
        let root_objects = storage.list_objects(None).await?;
        let download_folder = root_objects
            .iter()
            .find(|obj| {
                obj.is_folder()
                    && (obj.filename.eq_ignore_ascii_case("download")
                        || obj.filename.eq_ignore_ascii_case("downloads"))
            })
            .ok_or("Could not find Download folder on device")?;
        // Create the benchmark temp folder inside Download.
        let bench_folder_handle = storage
            .create_folder(Some(download_folder.handle), "mtp-bench-tmp")
            .await?;

        Ok(Self {
            device,
            storage_id,
            bench_folder_handle,
            description,
        })
    }

    /// Helper: get a fresh Storage reference for the stored storage ID.
    async fn storage(&self) -> Result<mtp_rs::mtp::Storage, Box<dyn std::error::Error>> {
        Ok(self.device.storage(self.storage_id).await?)
    }

    /// Upload `size` bytes of [`FILL_BYTE`] as `filename` into the benchmark folder, streamed in
    /// 1 MiB chunks so a 4 GB file costs no more memory than a 1 MB one. Returns the handle.
    pub async fn upload(
        &self,
        filename: &str,
        size: u64,
    ) -> Result<ObjectHandle, Box<dyn std::error::Error>> {
        let storage = self.storage().await?;
        let info = NewObjectInfo::file(filename, size);
        let handle = storage
            .upload(Some(self.bench_folder_handle), info, FillStream::new(size))
            .await?;
        Ok(handle)
    }

    /// Download the file at `handle`, returning all bytes.
    pub async fn download(
        &self,
        handle: ObjectHandle,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let storage = self.storage().await?;
        let data = storage.download_to_vec(handle).await?;
        Ok(data)
    }

    /// Held-open streaming download of the whole file, discarding chunks. Returns the byte count.
    pub async fn download_stream(
        &self,
        handle: ObjectHandle,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let storage = self.storage().await?;
        let mut download = storage
            .download_with_pacing(handle, mtp_rs::ByteRange::Full, DownloadPacing::HeldOpen)
            .await?;
        let mut total = 0u64;
        while let Some(chunk) = download.next_chunk().await {
            total += chunk?.len() as u64;
        }
        Ok(total)
    }

    /// Paced download of the whole file, discarding chunks. Returns the byte count.
    pub async fn download_paced(
        &self,
        handle: ObjectHandle,
        pacing: DownloadPacing,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let storage = self.storage().await?;
        let mut download = storage
            .download_with_pacing(handle, mtp_rs::ByteRange::Full, pacing)
            .await?;
        let mut total = 0u64;
        while let Some(chunk) = download.next_chunk().await {
            total += chunk?.len() as u64;
        }
        Ok(total)
    }

    /// Download the whole file with `pacing`, discarding chunks, and once at least half of it
    /// has arrived, list the storage root on the same session, the way a file manager refreshes
    /// a folder while a copy runs. Both run in this one task: the listing queues for the device
    /// while the download keeps reading, so it gets in at the next window boundary (or, held
    /// open, when the download ends).
    pub async fn download_with_mid_list(
        &self,
        handle: ObjectHandle,
        size: u64,
        pacing: DownloadPacing,
    ) -> Result<MidListTiming, Box<dyn std::error::Error>> {
        let storage = self.storage().await?;
        let start = Instant::now();
        let mut download = storage
            .download_with_pacing(handle, mtp_rs::ByteRange::Full, pacing)
            .await?;
        // A `Cell`, so the listing can read how far the download got when it answered.
        let received = Cell::new(0u64);
        while received.get() < size / 2 {
            match download.next_chunk().await {
                Some(chunk) => received.set(received.get() + chunk?.len() as u64),
                None => break,
            }
        }
        let issued_at = received.get();
        let list = async {
            let t = Instant::now();
            storage
                .list_objects(None)
                .await
                .map(|_| (t.elapsed(), received.get()))
        };
        let rest = async {
            while let Some(chunk) = download.next_chunk().await {
                received.set(received.get() + chunk?.len() as u64);
            }
            Ok::<_, mtp_rs::Error>(())
        };
        let (list, rest) = futures::join!(list, rest);
        let (list_latency, answered_at) = list?;
        rest?;
        let received = received.get();
        Ok(MidListTiming {
            answered_at,
            download: start.elapsed(),
            list_latency,
            bytes: received,
            issued_at,
        })
    }

    /// List objects in the root of the first storage. Returns the count.
    pub async fn list_objects(&self) -> Result<usize, Box<dyn std::error::Error>> {
        let storage = self.storage().await?;
        let objects = storage.list_objects(None).await?;
        Ok(objects.len())
    }

    /// Delete a single object.
    pub async fn delete(&self, handle: ObjectHandle) -> Result<(), Box<dyn std::error::Error>> {
        let storage = self.storage().await?;
        storage.delete(handle).await?;
        Ok(())
    }

    /// Clean up the benchmark folder and close the device.
    pub async fn cleanup(self) -> Result<(), Box<dyn std::error::Error>> {
        // Delete all objects inside the benchmark folder first.
        let storage = self.device.storage(self.storage_id).await?;
        let objects = storage.list_objects(Some(self.bench_folder_handle)).await?;
        for obj in &objects {
            storage.delete(obj.handle).await?;
        }

        // Delete the benchmark folder itself.
        storage.delete(self.bench_folder_handle).await?;

        // Close the device connection.
        self.device.close().await?;
        Ok(())
    }

    /// Human-readable device description (manufacturer + model).
    pub fn device_description(&self) -> &str {
        &self.description
    }
}

/// Open the device with this serial, or with no serial the only MTP device connected. Several
/// devices and no serial is an error rather than a pick: opening the wrong phone could disturb
/// whatever else is using it.
///
/// On Windows a phone is opened through WPD, whose serial is the MTP one (a Pixel's is 32 hex
/// digits), not the USB serial `mtp-rs devices` prints and `--serial` takes. So there a serial is
/// looked up in the USB listing and opened by its location, which `MtpDeviceBuilder` maps to the
/// WPD device by VID/PID (and the USB serial, when two identical models are plugged in). A serial
/// the listing doesn't know is tried as a WPD serial. Without a serial, the one listed device is
/// opened by location too, the way the CLI does, so the report's link speed (from the listing)
/// is the opened device's.
pub async fn open_device(serial: Option<&str>) -> Result<MtpDevice, Box<dyn std::error::Error>> {
    #[cfg(windows)]
    {
        let listed = MtpDevice::list_devices()?;
        let location = match serial {
            Some(s) => listed
                .iter()
                .find(|d| d.serial_number.as_deref() == Some(s))
                .map(|d| d.location_id),
            None => {
                check_single_device(listed.len())?;
                Some(listed[0].location_id)
            }
        };
        match (location, serial) {
            (Some(location), _) => Ok(MtpDevice::open_by_location(location).await?),
            (None, Some(s)) => Ok(MtpDevice::open_by_serial(s).await?),
            (None, None) => unreachable!("no serial means one listed device"),
        }
    }
    #[cfg(not(windows))]
    match serial {
        Some(s) => Ok(MtpDevice::open_by_serial(s).await?),
        None => {
            let listed = MtpDevice::list_devices()?;
            check_single_device(listed.len())?;
            Ok(MtpDevice::open_first().await?)
        }
    }
}

/// Without `--serial`, exactly one device must be connected.
pub fn check_single_device(count: usize) -> Result<(), String> {
    match count {
        0 => Err("No MTP device found".into()),
        1 => Ok(()),
        n => Err(format!(
            "{n} MTP devices connected; pick one with --serial (`mtp-rs devices` lists them)"
        )),
    }
}

/// What one [`MtpRsBackend::download_with_mid_list`] run measured.
#[derive(Debug, Clone, Copy)]
pub struct MidListTiming {
    /// The whole download, from the call to the last byte, the listing's turn included.
    pub download: Duration,
    /// From issuing the listing to its answer.
    pub list_latency: Duration,
    /// Bytes the download delivered.
    pub bytes: u64,
    /// Bytes already delivered when the listing was issued.
    pub issued_at: u64,
    /// Bytes delivered when the listing answered: the file's size when it had to wait for the
    /// whole download (held open), less when it got in between windows.
    pub answered_at: u64,
}

// ---------------------------------------------------------------------------
// Upload source. The mtp-rs `upload` method takes a
// `Stream<Item = Result<Bytes, io::Error>> + Unpin`.
// ---------------------------------------------------------------------------

use bytes::Bytes;
use std::cell::Cell;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// The byte every test file is filled with.
pub const FILL_BYTE: u8 = 0xAB;

const FILL_CHUNK: usize = 1024 * 1024;

/// `size` bytes of [`FILL_BYTE`], as 1 MiB chunks that all share one buffer.
pub struct FillStream {
    chunk: Bytes,
    remaining: u64,
}

impl FillStream {
    /// A stream of `size` fill bytes.
    pub fn new(size: u64) -> Self {
        Self {
            chunk: Bytes::from(vec![FILL_BYTE; FILL_CHUNK]),
            remaining: size,
        }
    }
}

impl futures::Stream for FillStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        let n = self.remaining.min(FILL_CHUNK as u64) as usize;
        self.remaining -= n as u64;
        Poll::Ready(Some(Ok(self.chunk.slice(..n))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn fill_stream_yields_exactly_the_size_in_bounded_chunks() {
        for size in [
            0u64,
            1,
            FILL_CHUNK as u64,
            FILL_CHUNK as u64 + 1,
            3 * FILL_CHUNK as u64 - 7,
        ] {
            let chunks: Vec<Bytes> =
                futures::executor::block_on(FillStream::new(size).map(|c| c.unwrap()).collect());
            let total: u64 = chunks.iter().map(|c| c.len() as u64).sum();
            assert_eq!(total, size);
            assert!(chunks
                .iter()
                .all(|c| !c.is_empty() && c.len() <= FILL_CHUNK));
            assert!(chunks.iter().all(|c| c.iter().all(|&b| b == FILL_BYTE)));
        }
    }

    #[test]
    fn without_a_serial_only_a_lone_device_is_opened() {
        assert!(check_single_device(0).is_err());
        assert!(check_single_device(1).is_ok());
        assert!(check_single_device(2).unwrap_err().contains("--serial"));
    }

    /// A virtual device with a `Download` folder, like the phone the bench expects.
    async fn virtual_backend(dir: &std::path::Path) -> MtpRsBackend {
        use mtp_rs::virtual_device::{VirtualDeviceConfig, VirtualStorageConfig};
        std::fs::create_dir(dir.join("Download")).unwrap();
        std::fs::write(dir.join("sibling.txt"), b"something to list").unwrap();
        let device = MtpDevice::builder()
            .open_virtual(VirtualDeviceConfig {
                serial: format!("mtp-bench-{}", dir.display()),
                storages: vec![VirtualStorageConfig {
                    description: "Internal Storage".into(),
                    capacity: 1024 * 1024 * 1024,
                    backing_dir: dir.to_path_buf(),
                    read_only: false,
                }],
                event_poll_interval: Duration::ZERO,
                watch_backing_dirs: false,
                ..Default::default()
            })
            .await
            .unwrap();
        MtpRsBackend::with_device(device).await.unwrap()
    }

    const MIB: u64 = 1024 * 1024;

    #[tokio::test]
    async fn a_paced_download_lets_the_mid_download_listing_in_before_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let backend = virtual_backend(dir.path()).await;
        let size = 16 * MIB;
        let handle = backend.upload("big.bin", size).await.unwrap();

        let t = backend
            .download_with_mid_list(handle, size, DownloadPacing::FixedWindow(MIB as u32))
            .await
            .unwrap();
        assert_eq!(t.bytes, size);
        assert!(t.issued_at >= size / 2, "{t:?}");
        assert!(
            t.answered_at < size,
            "the listing should get in between windows: {t:?}"
        );
        // It waits for at most the window in flight.
        assert!(t.answered_at - t.issued_at <= MIB, "{t:?}");

        backend.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn a_held_open_download_makes_the_listing_wait_for_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let backend = virtual_backend(dir.path()).await;
        let size = 16 * MIB + 3;
        let handle = backend.upload("big.bin", size).await.unwrap();

        let t = backend
            .download_with_mid_list(handle, size, DownloadPacing::HeldOpen)
            .await
            .unwrap();
        assert_eq!(t.bytes, size);
        assert_eq!(t.answered_at, size, "{t:?}");

        backend.cleanup().await.unwrap();
    }
}
