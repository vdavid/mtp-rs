//! Benchmark backend using the pure-Rust mtp-rs crate.

use mtp_rs::mtp::MtpDevice;
use mtp_rs::mtp::NewObjectInfo;
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
    pub async fn connect() -> Result<Self, Box<dyn std::error::Error>> {
        let device = MtpDevice::open_first().await?;

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

    /// Upload `data` as `filename` into the benchmark folder. Returns the object handle.
    pub async fn upload(
        &self,
        filename: &str,
        data: &[u8],
    ) -> Result<ObjectHandle, Box<dyn std::error::Error>> {
        let storage = self.storage().await?;
        let info = NewObjectInfo::file(filename, data.len() as u64);

        // Build a single-item stream of bytes for the upload.
        let data_vec = data.to_vec();
        let stream = SingleChunkStream::new(data_vec);

        let handle = storage
            .upload(Some(self.bench_folder_handle), info, stream)
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
        let mut download = storage.download(handle, mtp_rs::ByteRange::Full).await?;
        let mut total = 0u64;
        while let Some(chunk) = download.next_chunk().await {
            total += chunk?.len() as u64;
        }
        Ok(total)
    }

    /// Windowed download of the whole file, discarding windows. Returns the byte count.
    pub async fn download_windowed(
        &self,
        handle: ObjectHandle,
        window_size: u32,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let storage = self.storage().await?;
        let mut download = storage
            .download_windowed(handle, mtp_rs::ByteRange::Full, window_size)
            .await?;
        let mut total = 0u64;
        while let Some(window) = download.next_window().await {
            total += window?.len() as u64;
        }
        Ok(total)
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

// ---------------------------------------------------------------------------
// Minimal single-chunk stream implementation. The mtp-rs `upload` method
// requires `Stream<Item = Result<Bytes, io::Error>> + Unpin`, so we provide
// a lightweight wrapper that yields all data in one chunk.
// ---------------------------------------------------------------------------

use bytes::Bytes;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A stream that yields a single chunk of bytes and then finishes.
struct SingleChunkStream {
    data: Option<Vec<u8>>,
}

impl SingleChunkStream {
    fn new(data: Vec<u8>) -> Self {
        Self { data: Some(data) }
    }
}

impl futures::Stream for SingleChunkStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.data.take() {
            Some(data) => Poll::Ready(Some(Ok(Bytes::from(data)))),
            None => Poll::Ready(None),
        }
    }
}

impl Unpin for SingleChunkStream {}
