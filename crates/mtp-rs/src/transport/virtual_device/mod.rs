//! Virtual MTP device transport for testing.
//!
//! This module provides a [`VirtualTransport`] that implements the [`Transport`] trait
//! using a local filesystem directory as its backing store. It speaks the full MTP/PTP
//! binary protocol, so the existing `PtpSession`, `MtpDevice`, and `Storage` types
//! work unchanged.
//!
//! Use this to test MTP client code without real USB hardware.
//!
//! # Example
//!
//! ```rust,no_run
//! use std::path::PathBuf;
//! use mtp_rs::MtpDevice;
//! use mtp_rs::transport::virtual_device::config::{VirtualDeviceConfig, VirtualStorageConfig};
//!
//! # async fn example() -> Result<(), mtp_rs::Error> {
//! let device = MtpDevice::builder()
//!     .open_virtual(VirtualDeviceConfig {
//!         manufacturer: "Google".into(),
//!         model: "Virtual Pixel 9".into(),
//!         serial: "virtual-001".into(),
//!         storages: vec![VirtualStorageConfig {
//!             description: "Internal Storage".into(),
//!             capacity: 64 * 1024 * 1024 * 1024,
//!             backing_dir: PathBuf::from("/tmp/mtp-test"),
//!             read_only: false,
//!         }],
//!         ..Default::default()
//!     })
//!     .await?;
//!
//! // Use the device exactly like a real one
//! for storage in device.storages().await? {
//!     for obj in storage.list_objects(None).await? {
//!         println!("{}", obj.filename);
//!     }
//! }
//! # Ok(())
//! # }
//! ```

mod builders;
pub mod config;
mod handlers;
pub mod registry;
mod state;
mod watcher;

use crate::ptp::{unpack_u16, unpack_u32};
use crate::transport::Transport;
use async_trait::async_trait;
use config::VirtualDeviceConfig;
use state::{PendingCommand, VirtualDeviceState};
pub use state::{RescanSummary, DROPPED_PATHS_CAP};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A transport that speaks MTP/PTP binary protocol against a local filesystem.
///
/// Created via `MtpDeviceBuilder::open_virtual()` or directly for lower-level use.
///
/// Internally, incoming `send_bulk` calls are parsed as MTP command/data containers.
/// The virtual device processes each operation (list files, read, write, delete, etc.)
/// against the configured backing directories and queues binary response containers
/// for the next `receive_bulk` call.
///
/// A background filesystem watcher detects out-of-band changes to the backing
/// directories and queues corresponding MTP events. The watcher is stopped
/// automatically when the transport is dropped.
pub struct VirtualTransport {
    state: Arc<Mutex<VirtualDeviceState>>,
    /// How long `receive_interrupt` waits when no events are pending.
    event_poll_interval: Duration,
    /// Serial number, used to unregister from the active-states registry on drop.
    serial: String,
    /// Filesystem watcher. Stops watching when dropped.
    _watcher: Option<notify::RecommendedWatcher>,
}

impl VirtualTransport {
    /// Create a new virtual transport from a device configuration.
    ///
    /// The backing directories in each storage config should already exist.
    /// When `config.watch_backing_dirs` is `true`, starts a background
    /// filesystem watcher for detecting out-of-band changes.
    #[must_use]
    pub fn new(config: VirtualDeviceConfig) -> Self {
        let event_poll_interval = config.event_poll_interval;
        let watch = config.watch_backing_dirs;
        let serial = config.serial.clone();
        let state = Arc::new(Mutex::new(VirtualDeviceState::new(config)));

        // Register the state so `rescan_virtual_device()` can find it.
        registry::register_active_state(serial.clone(), Arc::clone(&state));

        let watcher = if watch {
            watcher::start_fs_watcher(&state)
        } else {
            None
        };
        Self {
            state,
            event_poll_interval,
            serial,
            _watcher: watcher,
        }
    }
}

impl Drop for VirtualTransport {
    fn drop(&mut self) {
        registry::unregister_active_state(&self.serial);
    }
}

/// Container type constants.
const CONTAINER_TYPE_COMMAND: u16 = 1;
const CONTAINER_TYPE_DATA: u16 = 2;

#[async_trait]
impl Transport for VirtualTransport {
    async fn send_bulk(&self, data: &[u8]) -> Result<(), crate::PtpError> {
        if data.len() < 12 {
            return Err(crate::PtpError::invalid_data("container too small"));
        }

        let _length = unpack_u32(&data[0..4])?;
        let container_type = unpack_u16(&data[4..6])?;
        let code = unpack_u16(&data[6..8])?;
        let tx_id = unpack_u32(&data[8..12])?;

        let mut state = self.state.lock().unwrap();

        match container_type {
            CONTAINER_TYPE_COMMAND => {
                // Model a device wedged mid-session (#18): when armed via
                // `force_operation_wedge`, the operation reports `DeviceReset`
                // (one-shot) before it does anything. Only command containers
                // check it, so a data phase never lands on a device that already
                // reported the reset.
                if std::mem::take(&mut state.pending_operation_wedge) {
                    return Err(crate::PtpError::DeviceReset);
                }
                // Parse parameters (each u32, after the 12-byte header)
                let param_bytes = data.len() - 12;
                let param_count = param_bytes / 4;
                let mut params = Vec::with_capacity(param_count);
                for i in 0..param_count {
                    let offset = 12 + i * 4;
                    params.push(unpack_u32(&data[offset..])?);
                }

                // Check if this operation expects a data phase from the host.
                // If so, don't dispatch yet -- store the command and wait for data.
                let op = crate::ptp::OperationCode::from(code);
                if matches!(
                    op,
                    crate::ptp::OperationCode::SendObjectInfo
                        | crate::ptp::OperationCode::SendObject
                        | crate::ptp::OperationCode::SetObjectPropValue
                ) {
                    state.pending_command = Some(PendingCommand {
                        code,
                        tx_id,
                        params,
                    });
                } else {
                    handlers::dispatch(&mut state, code, tx_id, &params, None);
                }
            }
            CONTAINER_TYPE_DATA => {
                // This is the data phase for a previous command.
                match state.pending_command.take() {
                    Some(pending) => {
                        let payload = &data[12..]; // Skip data container header
                        handlers::dispatch(
                            &mut state,
                            pending.code,
                            pending.tx_id,
                            &pending.params,
                            Some(payload),
                        );
                    }
                    None => {
                        return Err(crate::PtpError::invalid_data(
                            "received data container without pending command",
                        ));
                    }
                }
            }
            _ => {
                return Err(crate::PtpError::invalid_data(format!(
                    "unexpected container type: {}",
                    container_type
                )));
            }
        }

        Ok(())
    }

    async fn receive_bulk(&self, _max_size: usize) -> Result<Vec<u8>, crate::PtpError> {
        let mut state = self.state.lock().unwrap();
        match state.response_queue.pop_front() {
            Some(data) => Ok(data),
            None => Err(crate::PtpError::invalid_data("no response available")),
        }
    }

    async fn receive_interrupt(&self) -> Result<Vec<u8>, crate::PtpError> {
        // Check for events (drop lock before any await)
        {
            let mut state = self.state.lock().unwrap();
            if let Some(event) = state.event_queue.pop_front() {
                return Ok(event);
            }
        }
        // No events: wait, then return Timeout
        futures_timer::Delay::new(self.event_poll_interval).await;
        Err(crate::PtpError::Timeout)
    }

    async fn cancel_transfer(
        &self,
        _transaction_id: u32,
        _idle_timeout: std::time::Duration,
    ) -> Result<(), crate::PtpError> {
        // Virtual device has no USB pipe to drain, so just clear any pending state.
        let mut state = self.state.lock().unwrap();
        state.pending_command = None;
        state.response_queue.clear();
        // Model the Samsung cancel wedge (#18): when armed via
        // `force_cancel_wedge`, report `DeviceReset` (one-shot), as the real USB
        // transport does after detecting the wedge and resetting the device.
        if std::mem::take(&mut state.pending_cancel_wedge) {
            return Err(crate::PtpError::DeviceReset);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::config::{VirtualDeviceConfig, VirtualStorageConfig};
    use crate::mtp::{ByteRange, MtpDevice, ObjectFormat};
    use std::time::Duration;

    fn test_config(dir: &std::path::Path) -> VirtualDeviceConfig {
        VirtualDeviceConfig {
            manufacturer: "TestCorp".into(),
            model: "Virtual Phone".into(),
            serial: "test-001".into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024, // 1 GB
                backing_dir: dir.to_path_buf(),
                read_only: false,
            }],
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        }
    }

    fn test_config_readonly(dir: &std::path::Path) -> VirtualDeviceConfig {
        VirtualDeviceConfig {
            manufacturer: "TestCorp".into(),
            model: "Virtual Phone".into(),
            serial: "test-ro".into(),
            storages: vec![VirtualStorageConfig {
                description: "Read-only Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: dir.to_path_buf(),
                read_only: true,
            }],
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        }
    }

    fn test_config_multi(dirs: &[&std::path::Path]) -> VirtualDeviceConfig {
        VirtualDeviceConfig {
            manufacturer: "TestCorp".into(),
            model: "Virtual Phone".into(),
            serial: "test-multi".into(),
            storages: dirs
                .iter()
                .enumerate()
                .map(|(i, d)| VirtualStorageConfig {
                    description: format!("Storage {}", i + 1),
                    capacity: 1024 * 1024 * 1024,
                    backing_dir: d.to_path_buf(),
                    read_only: false,
                })
                .collect(),
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        }
    }

    /// Helper to convert `&[u8]` to a `Stream<Item = Result<Bytes, io::Error>>`.
    fn bytes_stream(
        data: &[u8],
    ) -> impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin {
        futures::stream::once(futures::future::ok(bytes::Bytes::copy_from_slice(data)))
    }

    #[tokio::test]
    async fn open_virtual_and_list_storages() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        assert_eq!(storages.len(), 1);
        assert_eq!(storages[0].info().description, "Internal Storage");
        assert!(storages[0].info().total_capacity > 0);
    }

    #[tokio::test]
    async fn device_info_matches_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let info = device.device_info();

        assert_eq!(info.manufacturer, "TestCorp");
        assert_eq!(info.model, "Virtual Phone");
        assert_eq!(info.serial_number, "test-001");
        assert!(device.supports_rename());
    }

    #[tokio::test]
    async fn list_objects_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let objects = storages[0].list_objects(None).await.unwrap();

        assert!(objects.is_empty());
    }

    #[tokio::test]
    async fn list_objects_with_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hello world").unwrap();
        std::fs::write(dir.path().join("photo.jpg"), "fake jpeg data").unwrap();
        std::fs::create_dir(dir.path().join("Documents")).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let items = storages[0].list_objects(None).await.unwrap();

        assert_eq!(items.len(), 3);
        let names: Vec<&str> = items.iter().map(|i| i.filename.as_str()).collect();
        assert!(names.contains(&"hello.txt"));
        assert!(names.contains(&"photo.jpg"));
        assert!(names.contains(&"Documents"));

        // Verify folder detection
        let docs = items.iter().find(|i| i.filename == "Documents").unwrap();
        assert!(docs.is_folder());
        assert_eq!(docs.format, ObjectFormat::ASSOCIATION);

        // Verify file metadata
        let txt = items.iter().find(|i| i.filename == "hello.txt").unwrap();
        assert!(txt.is_file());
        assert_eq!(txt.size, 11); // "hello world" = 11 bytes
    }

    #[tokio::test]
    async fn download_file() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"test file content for download";
        std::fs::write(dir.path().join("test.txt"), content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let items = storages[0].list_objects(None).await.unwrap();
        let obj = &items[0];

        let data = storages[0].download_to_vec(obj.handle).await.unwrap();
        assert_eq!(data.as_slice(), content);
    }

    #[tokio::test]
    async fn download_partial_reads_byte_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let items = storages[0].list_objects(None).await.unwrap();
        let obj = &items[0];

        // Read from the beginning.
        let head = storages[0].read_range(obj.handle, 0, 100).await.unwrap();
        assert_eq!(head, &content[0..100]);

        // Read from the middle.
        let middle = storages[0].read_range(obj.handle, 500, 100).await.unwrap();
        assert_eq!(middle, &content[500..600]);

        // Read past the end: device returns whatever's left.
        let tail = storages[0].read_range(obj.handle, 990, 1000).await.unwrap();
        assert_eq!(tail, &content[990..1000]);
    }

    #[tokio::test]
    async fn download_partial_64_reads_byte_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let items = storages[0].list_objects(None).await.unwrap();
        let obj = &items[0];

        // Same scenarios as the 32-bit version, using the 64-bit op.
        let head = storages[0].read_range(obj.handle, 0, 100).await.unwrap();
        assert_eq!(head, &content[0..100]);

        let middle = storages[0].read_range(obj.handle, 500, 100).await.unwrap();
        assert_eq!(middle, &content[500..600]);

        let tail = storages[0].read_range(obj.handle, 990, 1000).await.unwrap();
        assert_eq!(tail, &content[990..1000]);
    }

    #[tokio::test]
    async fn download_partial_64_reassembles_offset_correctly() {
        // Verifies the lo/hi u32 → u64 round-trip. We can't realistically create a >4GB
        // file, so instead we test that an offset whose low u32 is 0 (i.e. an exact
        // multiple of 2^32) routes through the same code path with no truncation.
        // For a small file, any offset >= file length returns an empty read, which
        // still proves the u64 offset made it through correctly (if the high bits
        // were dropped, we'd incorrectly read from the start of the file).
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..100).map(|i| i as u8).collect();
        std::fs::write(dir.path().join("small.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let items = storages[0].list_objects(None).await.unwrap();
        let obj = &items[0];

        // Offset = 2^32 + 10. If the hi u32 were dropped, this would read from byte 10.
        let offset_beyond_4gb: u64 = (1u64 << 32) + 10;
        let data = storages[0]
            .read_range(obj.handle, offset_beyond_4gb, 50)
            .await
            .unwrap();
        assert!(
            data.is_empty(),
            "offset past EOF should return empty, got {} bytes; high offset bits may be getting dropped",
            data.len()
        );
    }

    /// Drain a `FileDownload` to a `Vec<u8>`, asserting no chunk errors.
    async fn collect_download(mut dl: crate::mtp::FileDownload) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = dl.next_chunk().await {
            out.extend_from_slice(&chunk.expect("chunk should not error"));
        }
        out
    }

    #[tokio::test]
    async fn download_stream_from_offset_returns_correct_tail() {
        let dir = tempfile::tempdir().unwrap();
        // 5000 bytes so the data container spans the streaming receive buffer
        // logic, with a recognizable byte pattern.
        let content: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        for offset in [0u64, 1, 100, 2500, 4999] {
            let dl = storages[0]
                .download(obj.handle, ByteRange::From(offset))
                .await
                .unwrap();
            assert_eq!(
                dl.size(),
                content.len() as u64,
                "size() must report the full object size, not the segment, at offset {offset}"
            );
            let got = collect_download(dl).await;
            assert_eq!(
                got,
                content[offset as usize..],
                "tail bytes wrong for offset {offset}"
            );
        }
    }

    #[tokio::test]
    async fn download_stream_from_offset_zero_matches_full_download() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..3333).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        let full = collect_download(
            storages[0]
                .download(obj.handle, ByteRange::Full)
                .await
                .unwrap(),
        )
        .await;
        let from_zero = collect_download(
            storages[0]
                .download(obj.handle, ByteRange::From(0))
                .await
                .unwrap(),
        )
        .await;

        assert_eq!(from_zero, full);
        assert_eq!(from_zero, content);
    }

    #[tokio::test]
    async fn download_stream_from_offset_at_size_is_empty_clean_eof() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"exactly this many bytes".to_vec();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        let mut dl = storages[0]
            .download(obj.handle, ByteRange::From(content.len() as u64))
            .await
            .unwrap();
        // Empty tail: the very first poll must be a clean EOF (None), not a hang
        // and not an error.
        assert!(
            dl.next_chunk().await.is_none(),
            "offset == size must yield zero chunks"
        );
        // The download holds the session lock until dropped (same contract as a
        // full download), so drop it before the follow-up op.
        drop(dl);

        // The session is still usable afterwards.
        let again = storages[0].list_objects(None).await.unwrap();
        assert_eq!(again.len(), 1);
    }

    #[tokio::test]
    async fn download_stream_from_offset_past_size_errors() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"small".to_vec();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // `FileDownload` isn't `Debug`, so match the result directly instead of
        // `expect_err`.
        match storages[0]
            .download(obj.handle, ByteRange::From(content.len() as u64 + 1))
            .await
        {
            Err(crate::mtp::Error::InvalidData { .. }) => {}
            Err(other) => panic!("expected InvalidData for offset past size, got {other:?}"),
            Ok(_) => panic!("offset past size must error, not return a stream (it would hang)"),
        }

        // No USB transaction was issued, so the session stays usable.
        assert_eq!(storages[0].list_objects(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancel_mid_partial_stream_leaves_session_usable() {
        use crate::mtp::DEFAULT_CANCEL_TIMEOUT;

        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..8000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        let mut dl = storages[0]
            .download(obj.handle, ByteRange::From(1000))
            .await
            .unwrap();
        // Pull one chunk, then cancel mid-stream (the resume use case: stop to
        // free the session).
        let first = dl.next_chunk().await.expect("first chunk").unwrap();
        assert!(!first.is_empty());
        dl.cancel(DEFAULT_CANCEL_TIMEOUT).await.unwrap();
        drop(dl);

        // A follow-up operation must work on the same session (cancel drained /
        // recovery realigned the pipe).
        let listed = storages[0].list_objects(None).await.unwrap();
        assert_eq!(listed.len(), 1);

        // And a fresh full resume from offset 0 still returns the whole file.
        let full = collect_download(
            storages[0]
                .download(obj.handle, ByteRange::From(0))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(full, content);
    }

    #[tokio::test]
    async fn an_undescribable_object_leaves_its_siblings_readable() {
        use crate::mtp::ListingItem;

        let dir = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }

        let serial = "object-info-error-22";
        let config = test_config_with_serial(dir.path(), serial);
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        // Pick a real object, then make the device refuse to describe it while
        // leaving it present: exactly what Sphaira does for one handle out of 50.
        let all = storages[0].list_objects(None).await.unwrap();
        assert_eq!(all.len(), 3);
        let victim = all.iter().find(|o| o.filename == "b.txt").unwrap().handle;
        assert!(crate::force_object_info_error(serial, victim, None));

        // The plain listing keeps the siblings instead of failing outright.
        let objects = storages[0].list_objects(None).await.unwrap();
        let mut names: Vec<_> = objects.iter().map(|o| o.filename.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["a.txt", "c.txt"]);

        // The collecting API additionally says which handle it couldn't read, so a
        // consumer can tell "2 files" from "2 files and one we couldn't see".
        let collection = storages[0].collect_objects(None).await.unwrap();
        assert_eq!(collection.objects.len(), 2);
        assert_eq!(collection.skipped.len(), 1);
        assert_eq!(collection.skipped[0].handle, victim);

        // And the streaming API reports it as an item, not an error.
        let mut listing = storages[0].list_objects_stream(None).await.unwrap();
        let mut streamed = 0;
        let mut skipped = 0;
        while let Some(item) = listing.next().await {
            match item.expect("a skipped object must not surface as a fatal error") {
                ListingItem::Object(_) => streamed += 1,
                ListingItem::Skipped(s) => {
                    assert_eq!(s.handle, victim);
                    skipped += 1;
                }
            }
        }
        assert_eq!((streamed, skipped), (2, 1));

        // Clearing the hook restores the object, proving nothing was destroyed.
        assert!(crate::clear_object_info_errors(serial));
        assert_eq!(storages[0].list_objects(None).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_folder_of_undescribable_objects_is_an_error_not_an_empty_folder() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }

        let serial = "object-info-error-all-22";
        let config = test_config_with_serial(dir.path(), serial);
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        for obj in storages[0].list_objects(None).await.unwrap() {
            assert!(crate::force_object_info_error(serial, obj.handle, None));
        }

        // A device that describes nothing is broken, not the owner of an empty
        // folder. Reporting `Ok(vec![])` here would read as "everything was
        // deleted" to anything syncing.
        let err = storages[0]
            .list_objects(None)
            .await
            .expect_err("a folder where every handle failed must not read as empty");
        assert!(
            matches!(err, crate::mtp::Error::Other { ref detail } if detail == "GeneralError"),
            "expected the device's own error to survive, got {err:?}"
        );
    }

    #[tokio::test]
    async fn cancel_wedge_surfaces_device_reset() {
        use crate::mtp::DEFAULT_CANCEL_TIMEOUT;

        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..8000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let serial = "cancel-wedge-18";
        let config = test_config_with_serial(dir.path(), serial);
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // Arm the one-shot cancel wedge (#18).
        assert!(crate::force_cancel_wedge(serial));

        let mut dl = storages[0]
            .download(obj.handle, ByteRange::Full)
            .await
            .unwrap();
        let _ = dl.next_chunk().await; // put a transfer in flight
        let err = dl
            .cancel(DEFAULT_CANCEL_TIMEOUT)
            .await
            .expect_err("a wedged cancel must report the reset, not a false success");
        assert!(
            matches!(err, crate::mtp::Error::DeviceReset),
            "expected Error::DeviceReset, got {err:?}"
        );
        drop(dl);

        // The wedge is one-shot: a subsequent cancel is healthy again, proving the
        // flag doesn't stick and the DeviceReset was the modeled wedge, not a
        // permanent state.
        let mut dl2 = storages[0]
            .download(obj.handle, ByteRange::Full)
            .await
            .unwrap();
        let _ = dl2.next_chunk().await;
        dl2.cancel(DEFAULT_CANCEL_TIMEOUT)
            .await
            .expect("second cancel should be healthy");
    }

    #[tokio::test]
    async fn operation_wedge_surfaces_device_reset_without_a_cancel() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"payload").unwrap();

        let serial = "operation-wedge-18";
        let config = test_config_with_serial(dir.path(), serial);
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // A consumer that never calls `cancel()` still reaches `DeviceReset`
        // through the recovery drain, so it needs a hook that doesn't ride on a
        // cancel to test its reopen path.
        assert!(crate::force_operation_wedge(serial));

        let err = storages[0]
            .get_object_info(obj.handle)
            .await
            .expect_err("an armed operation wedge must surface the reset");
        assert!(
            matches!(err, crate::mtp::Error::DeviceReset),
            "expected Error::DeviceReset, got {err:?}"
        );

        // One-shot, like the cancel wedge: the next operation is healthy again.
        let again = storages[0]
            .get_object_info(obj.handle)
            .await
            .expect("the wedge is one-shot");
        assert_eq!(again.filename, "data.bin");
    }

    #[tokio::test]
    async fn a_wedged_root_listing_reports_the_reset_it_hit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"payload").unwrap();

        let serial = "root-listing-wedge-18";
        let config = test_config_with_serial(dir.path(), serial);
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        assert!(crate::force_operation_wedge(serial));

        // The root-listing fast path asks for parent=0xFFFFFFFF first and falls
        // back to parent=0 when the device DECLINES it. A wedged session is not
        // a decline: retrying hammers a device the #18 notes say re-wedges under
        // exactly that treatment, and reporting the second attempt's error hides
        // the `DeviceReset` a consumer needs in order to reopen.
        //
        // Pre-fix this returned `Ok`: the one-shot wedge was swallowed by the
        // fast path and the parent=0 attempt succeeded.
        let err = storages[0]
            .list_objects(None)
            .await
            .expect_err("a wedged root listing must report the reset");
        assert!(
            matches!(err, crate::mtp::Error::DeviceReset),
            "expected Error::DeviceReset, got {err:?}"
        );

        // The one-shot was spent by that single attempt, so the device is
        // healthy again: proof the fallback never issued a second roundtrip.
        let objects = storages[0].list_objects(None).await.unwrap();
        assert_eq!(objects.len(), 1);
    }

    #[test]
    fn operation_wedge_reports_an_unknown_serial() {
        assert!(!crate::force_operation_wedge("no-such-device"));
    }

    // ---- Windowed downloads (session-freeing window-by-window reads) ----

    /// Drain a `WindowedDownload` to a `Vec<u8>`, asserting no window errors.
    async fn collect_windowed(mut dl: crate::mtp::WindowedDownload) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(window) = dl.next_window().await {
            out.extend_from_slice(&window.expect("window should not error"));
        }
        out
    }

    #[tokio::test]
    async fn windowed_download_reassembles_to_full_file() {
        let dir = tempfile::tempdir().unwrap();
        // 5000 bytes, a recognizable pattern; window of 1024 forces ~5 windows
        // (with a short final one) so reassembly spans many transactions.
        let content: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        let dl = storages[0]
            .download_windowed(obj.handle, ByteRange::Full, 1024)
            .await
            .unwrap();
        assert_eq!(dl.size(), content.len() as u64, "size() is the full object");
        let windowed = collect_windowed(dl).await;
        assert_eq!(windowed, content, "windowed reassembly must equal the file");

        // Equals a plain full download too.
        let full = storages[0].download_to_vec(obj.handle).await.unwrap();
        assert_eq!(windowed, full);

        // And equals a manual download_partial_64 reassembly (the primitive the
        // window loop is built on).
        let mut manual = Vec::new();
        let mut off = 0u64;
        while off < content.len() as u64 {
            let chunk = storages[0].read_range(obj.handle, off, 1024).await.unwrap();
            if chunk.is_empty() {
                break;
            }
            off += chunk.len() as u64;
            manual.extend_from_slice(&chunk);
        }
        assert_eq!(windowed, manual);
    }

    #[tokio::test]
    async fn windowed_download_works_on_32bit_only_device() {
        // A camera-like device that advertises GetPartialObject (32-bit) but NOT
        // GetPartialObject64 (the Lumix DMC-TZ61 case, #12). Windowed/ranged reads
        // must fall back to the 32-bit op and still return byte-exact data, rather
        // than failing Unsupported.
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = VirtualDeviceConfig {
            serial: "test-32bit-partial".into(),
            supports_partial_object_64: false,
            ..test_config(dir.path())
        };
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        // The capability probe conflates 32/64 into one flag, so it stays true here.
        assert!(device.capabilities().supports_partial_download);
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // Windowed read (goes through read_range -> 32-bit GetPartialObject).
        let dl = storages[0]
            .download_windowed(obj.handle, ByteRange::Full, 1024)
            .await
            .unwrap();
        assert_eq!(dl.size(), content.len() as u64);
        assert_eq!(collect_windowed(dl).await, content);

        // Ranged streaming download from an offset (goes through download() ->
        // 32-bit GetPartialObject) returns the correct tail. collect_download
        // consumes the FileDownload by value, releasing the one-per-device
        // session lock before the next operation.
        let offset = 1234u64;
        let ranged = storages[0]
            .download(obj.handle, ByteRange::From(offset))
            .await
            .unwrap();
        assert_eq!(ranged.size(), content.len() as u64);
        assert_eq!(collect_download(ranged).await, content[offset as usize..]);

        // And a plain read_range with an explicit length.
        let mid = storages[0].read_range(obj.handle, 100, 50).await.unwrap();
        assert_eq!(mid, content[100..150]);
    }

    #[tokio::test]
    async fn windowed_download_default_window_matches_full() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // The default 8 MiB window dwarfs this file, so it comes back in one
        // window, still byte-exact.
        let dl = storages[0]
            .download_windowed_default(obj.handle)
            .await
            .unwrap();
        assert_eq!(collect_windowed(dl).await, content);
    }

    #[tokio::test]
    async fn windowed_download_from_offset_returns_correct_tail() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        for offset in [0u64, 1, 100, 2500, 4999] {
            let dl = storages[0]
                .download_windowed(obj.handle, ByteRange::From(offset), 512)
                .await
                .unwrap();
            assert_eq!(
                dl.size(),
                content.len() as u64,
                "size() must report the full object size at offset {offset}"
            );
            assert_eq!(dl.offset(), offset, "offset() starts at the resume point");
            let got = collect_windowed(dl).await;
            assert_eq!(
                got,
                content[offset as usize..],
                "tail bytes wrong for offset {offset}"
            );
        }
    }

    #[tokio::test]
    async fn windowed_download_offset_zero_is_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..3333).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        let from_zero = collect_windowed(
            storages[0]
                .download_windowed(obj.handle, ByteRange::From(0), 256)
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(from_zero, content);
    }

    #[tokio::test]
    async fn windowed_download_at_size_first_window_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"exactly this many bytes".to_vec();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        let mut dl = storages[0]
            .download_windowed(obj.handle, ByteRange::From(content.len() as u64), 64)
            .await
            .unwrap();
        // offset == size: the first window is a clean None, no read issued.
        assert!(
            dl.next_window().await.is_none(),
            "offset == size must yield zero windows"
        );

        // Nothing is held between windows, so the session is immediately usable
        // (no cancel/drop dance needed).
        assert_eq!(storages[0].list_objects(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn windowed_download_past_size_errors() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"small".to_vec();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // `WindowedDownload` isn't `Debug`, so match the result directly.
        match storages[0]
            .download_windowed(obj.handle, ByteRange::From(content.len() as u64 + 1), 64)
            .await
        {
            Err(crate::mtp::Error::InvalidData { .. }) => {}
            Err(other) => panic!("expected InvalidData for offset past size, got {other:?}"),
            Ok(_) => panic!("offset past size must error, not return a download"),
        }

        // No USB transaction issued, so the session stays usable.
        assert_eq!(storages[0].list_objects(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn windowed_download_short_final_window_exact_total() {
        let dir = tempfile::tempdir().unwrap();
        // 1000 bytes with a 256-byte window: 3 full + a 232-byte final window.
        let content: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        let mut dl = storages[0]
            .download_windowed(obj.handle, ByteRange::Full, 256)
            .await
            .unwrap();
        let mut sizes = Vec::new();
        let mut total = 0usize;
        while let Some(window) = dl.next_window().await {
            let bytes = window.unwrap();
            total += bytes.len();
            sizes.push(bytes.len());
        }
        assert_eq!(total, 1000);
        assert_eq!(sizes, vec![256, 256, 256, 232], "clean short final window");
    }

    #[tokio::test]
    async fn windowed_download_empty_file_first_window_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.bin"), b"").unwrap();

        let config = test_config_with_serial(dir.path(), "windowed-empty-001");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();
        assert_eq!(obj.size, 0);

        // Arm a stall on any partial read so we can prove NO read is issued for an
        // empty file: if next_window wrongly issued a read it would consume this
        // cap, but it should short-circuit on offset >= size first.
        assert!(crate::force_partial_read_caps(
            "windowed-empty-001",
            vec![0]
        ));

        let mut dl = storages[0]
            .download_windowed(obj.handle, ByteRange::Full, 64)
            .await
            .unwrap();
        assert!(
            dl.next_window().await.is_none(),
            "empty file: first window is a clean None"
        );

        // Prove no read was issued: the armed cap is still pending. Issue one real
        // partial read now: it pops the still-armed cap and comes back empty,
        // which only holds if next_window() left the cap untouched.
        let probe = storages[0].read_range(obj.handle, 0, 64).await.unwrap();
        assert!(probe.is_empty(), "the still-armed cap fires on this read");
    }

    #[tokio::test]
    async fn windowed_download_zero_bytes_before_eof_errors() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..2000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config_with_serial(dir.path(), "windowed-stall-001");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        let mut dl = storages[0]
            .download_windowed(obj.handle, ByteRange::Full, 512)
            .await
            .unwrap();
        // First window reads fine.
        let first = dl.next_window().await.expect("first window").unwrap();
        assert_eq!(first.len(), 512);

        // Force the NEXT read to return 0 bytes mid-file (a device stall).
        assert!(crate::force_partial_read_caps(
            "windowed-stall-001",
            vec![0]
        ));
        match dl.next_window().await {
            Some(Err(crate::mtp::Error::InvalidData { .. })) => {}
            Some(Err(other)) => panic!("expected InvalidData stall error, got {other:?}"),
            Some(Ok(b)) => panic!("expected a stall error, got {} bytes", b.len()),
            None => panic!("a 0-byte read mid-file must be an error, NOT a clean EOF"),
        }
    }

    #[tokio::test]
    async fn windowed_download_short_mid_file_read_advances_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..2000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config_with_serial(dir.path(), "windowed-short-001");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // Force the first two reads to come back short (100, then 50 real bytes)
        // even though the window asks for 512, a legal partial read. The download
        // must advance by what actually arrived, so the result stays byte-exact.
        assert!(crate::force_partial_read_caps(
            "windowed-short-001",
            vec![100, 50]
        ));

        let mut dl = storages[0]
            .download_windowed(obj.handle, ByteRange::Full, 512)
            .await
            .unwrap();
        let w1 = dl.next_window().await.unwrap().unwrap();
        assert_eq!(w1.len(), 100, "first read clamped to 100 bytes");
        assert_eq!(dl.offset(), 100, "offset advanced by bytes returned");
        let w2 = dl.next_window().await.unwrap().unwrap();
        assert_eq!(w2.len(), 50, "second read clamped to 50 bytes");
        assert_eq!(dl.offset(), 150);

        // Remaining windows are full-size; the whole thing reassembles exactly.
        let mut got = w1;
        got.extend_from_slice(&w2);
        got.extend(collect_windowed(dl).await);
        assert_eq!(got, content, "short mid-file reads stay byte-exact");
    }

    #[tokio::test]
    async fn windowed_download_session_free_between_windows() {
        // The headline property: nothing is held between windows, so another
        // device operation succeeds BETWEEN two next_window() calls.
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..4000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("big.bin"), &content).unwrap();
        std::fs::write(dir.path().join("sibling.txt"), b"hi").unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let big = storages[0]
            .list_objects(None)
            .await
            .unwrap()
            .into_iter()
            .find(|o| o.filename == "big.bin")
            .unwrap();

        let mut dl = storages[0]
            .download_windowed(big.handle, ByteRange::Full, 512)
            .await
            .unwrap();

        // Read one window...
        let w1 = dl.next_window().await.expect("first window").unwrap();
        assert_eq!(w1.len(), 512);

        // ...now, WITHOUT cancelling or dropping the download, run a full listing
        // on the SAME session. This is the whole point: a held-open download_stream
        // would deadlock/serialize here; the windowed read leaves the session free.
        let listed = storages[0].list_objects(None).await.unwrap();
        assert_eq!(listed.len(), 2, "listing succeeds between windows");

        // The download then continues correctly from where it left off.
        let mut got = w1;
        got.extend(collect_windowed(dl).await);
        assert_eq!(
            got, content,
            "download resumes byte-exact after the listing"
        );
    }

    #[tokio::test]
    async fn upload_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        let info = crate::mtp::NewObjectInfo::file("uploaded.txt", 12);
        let handle = storages[0]
            .upload(None, info, bytes_stream(b"hello upload"))
            .await
            .unwrap();

        // Verify file exists on disk
        let path = dir.path().join("uploaded.txt");
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello upload");

        // Verify we can download it back
        let data = storages[0].download_to_vec(handle).await.unwrap();
        assert_eq!(data.as_slice(), b"hello upload");
    }

    #[tokio::test]
    async fn upload_to_subfolder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Music")).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        // List root to get the Music folder handle
        let items = storages[0].list_objects(None).await.unwrap();
        let music = items.iter().find(|i| i.filename == "Music").unwrap();
        assert!(music.is_folder());

        // Upload a file into Music
        let info = crate::mtp::NewObjectInfo::file("song.mp3", 5);
        storages[0]
            .upload(Some(music.handle), info, bytes_stream(b"audio"))
            .await
            .unwrap();

        assert!(dir.path().join("Music/song.mp3").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Music/song.mp3")).unwrap(),
            "audio"
        );
    }

    /// A stream that yields one good chunk, then an `io::Error`, simulating a
    /// source that fails partway through (disk read error, dropped network mount).
    fn failing_stream(
        good: &[u8],
    ) -> impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin {
        let good = bytes::Bytes::copy_from_slice(good);
        futures::stream::iter(vec![
            Ok(good),
            Err(std::io::Error::other("source blew up mid-stream")),
        ])
    }

    #[tokio::test]
    async fn upload_surfaces_partial_handle_on_midstream_error() {
        use crate::mtp::UploadError;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Download")).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        // Find the Download folder to upload into (avoids root-creation quirks).
        let items = storages[0].list_objects(None).await.unwrap();
        let download = items.iter().find(|i| i.filename == "Download").unwrap();

        // Claim the file is 100 bytes, but the stream fails after one short chunk.
        let info = crate::mtp::NewObjectInfo::file("partial.bin", 100);
        let err = storages[0]
            .upload(Some(download.handle), info, failing_stream(b"abc"))
            .await
            .expect_err("upload should fail when the source stream errors mid-stream");

        // The created handle must be surfaced so the caller can clean up or resume.
        let UploadError { source, partial } = err;
        assert!(
            matches!(source, crate::mtp::Error::Io { .. }),
            "expected the underlying I/O error, got {source:?}"
        );
        let handle = partial.expect("partial handle must be Some after SendObjectInfo succeeded");

        // The library must NOT have auto-deleted the object: it's a real handle on
        // the device, so resuming the data phase against it stays possible.
        let info = storages[0]
            .get_object_info(handle)
            .await
            .expect("the partially-written object should still exist at the surfaced handle");
        assert_eq!(info.filename, "partial.bin");

        // And the caller CAN clean it up with the surfaced handle.
        storages[0]
            .delete(handle)
            .await
            .expect("the surfaced handle should be deletable");
        assert!(
            storages[0].get_object_info(handle).await.is_err(),
            "object should be gone after delete"
        );
    }

    #[tokio::test]
    async fn upload_surfaces_partial_handle_on_cancel() {
        use crate::mtp::UploadError;
        use std::ops::ControlFlow;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Download")).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        let items = storages[0].list_objects(None).await.unwrap();
        let download = items.iter().find(|i| i.filename == "Download").unwrap();

        let info = crate::mtp::NewObjectInfo::file("cancelled.bin", 5);
        // Break from the progress callback on the first chunk -> Error::Cancelled.
        let err = storages[0]
            .upload_with_progress(
                Some(download.handle),
                info,
                bytes_stream(b"hello"),
                |_progress| ControlFlow::Break(()),
            )
            .await
            .expect_err("cancelled upload should fail");

        let UploadError { source, partial } = err;
        assert!(
            matches!(source, crate::mtp::Error::Cancelled),
            "cancellation must map to Error::Cancelled, got {source:?}"
        );
        assert!(
            partial.is_some(),
            "partial handle must be surfaced on cancellation too"
        );
    }

    #[tokio::test]
    async fn upload_success_returns_handle() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        let info = crate::mtp::NewObjectInfo::file("complete.txt", 12);
        let handle = storages[0]
            .upload(None, info, bytes_stream(b"hello upload"))
            .await
            .expect("a full upload should succeed");

        // Object is present and complete at the returned handle.
        let data = storages[0].download_to_vec(handle).await.unwrap();
        assert_eq!(data.as_slice(), b"hello upload");
    }

    #[tokio::test]
    async fn upload_to_readonly_storage_has_no_partial() {
        use crate::mtp::UploadError;

        let dir = tempfile::tempdir().unwrap();
        let config = test_config_readonly(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        // SendObjectInfo itself is rejected (read-only), so no object is created.
        let info = crate::mtp::NewObjectInfo::file("nope.txt", 4);
        let err: UploadError = storages[0]
            .upload(None, info, bytes_stream(b"data"))
            .await
            .expect_err("upload to read-only storage should fail");
        assert!(
            err.partial.is_none(),
            "no object is created when SendObjectInfo fails, so partial must be None"
        );
    }

    #[tokio::test]
    async fn rekey_object_invalidates_old_handle_then_relist_and_upload_recover() {
        // Reproduces the Android "stale cached handle" quirk end-to-end: a host
        // lists a folder and caches its handle, the device re-keys that handle
        // across a (simulated) media rescan, and the host's NEXT upload into the
        // cached handle is rejected, but a re-list surfaces the new handle and
        // an upload against it lands. This is the device-side behavior cmdr's
        // upload self-heal/retry depends on; before `rekey_virtual_object` there
        // was no way to produce it against the virtual device.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Download")).unwrap();

        let config = test_config_with_serial(dir.path(), "rekey-test-001");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        // Host lists root and caches the Download handle.
        let items = storages[0].list_objects(None).await.unwrap();
        let cached_handle = items
            .iter()
            .find(|i| i.filename == "Download")
            .unwrap()
            .handle;

        // Device re-keys Download out from under the host.
        let (reported_old, new_handle) =
            crate::rekey_virtual_object("rekey-test-001", std::path::Path::new("Download"))
                .expect("a listed object must be re-keyable");
        // `rekey_virtual_object` reports low-level `ptp::ObjectHandle`s (u32),
        // while the high-level listing yields neutral `mtp::ObjectHandle`s (u64);
        // they carry the same numeric id, so compare on the raw value.
        assert_eq!(
            u64::from(reported_old.0),
            cached_handle.0,
            "rekey should report the handle the host had cached"
        );
        assert_ne!(
            u64::from(new_handle.0),
            cached_handle.0,
            "rekey must assign a fresh handle"
        );

        // Uploading into the now-stale cached handle is rejected exactly like a
        // real device: InvalidParentObject at SendObjectInfo, no partial created.
        let err = storages[0]
            .upload(
                Some(cached_handle),
                crate::mtp::NewObjectInfo::file("after.txt", 5),
                bytes_stream(b"hello"),
            )
            .await
            .expect_err("uploading into a re-keyed (stale) parent handle must fail");
        assert!(
            matches!(err.source, crate::mtp::Error::StaleHandle),
            "expected an InvalidParentObject rejection (neutral StaleHandle), got {:?}",
            err.source
        );
        assert!(
            err.partial.is_none(),
            "SendObjectInfo was rejected, so no partial object exists"
        );

        // A fresh listing surfaces the NEW handle for the same folder...
        let items = storages[0].list_objects(None).await.unwrap();
        let relisted_handle = items
            .iter()
            .find(|i| i.filename == "Download")
            .unwrap()
            .handle;
        assert_eq!(
            relisted_handle.0,
            u64::from(new_handle.0),
            "re-listing the parent must surface the re-keyed handle"
        );

        // ...and uploading into the refreshed handle lands in the same folder.
        let handle = storages[0]
            .upload(
                Some(relisted_handle),
                crate::mtp::NewObjectInfo::file("after.txt", 5),
                bytes_stream(b"hello"),
            )
            .await
            .expect("upload into the refreshed handle should succeed");
        assert_eq!(
            storages[0]
                .download_to_vec(handle)
                .await
                .unwrap()
                .as_slice(),
            b"hello"
        );
        assert!(
            dir.path().join("Download/after.txt").exists(),
            "the recovered upload must land inside the (still-present) Download folder"
        );
    }

    #[tokio::test]
    async fn rekey_unknown_path_or_serial_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Download")).unwrap();
        let config = test_config_with_serial(dir.path(), "rekey-test-none");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        storages[0].list_objects(None).await.unwrap();

        // Unknown serial -> None.
        assert!(
            crate::rekey_virtual_object("no-such-serial", std::path::Path::new("Download"))
                .is_none()
        );
        // Known device, untracked path -> None.
        assert!(
            crate::rekey_virtual_object("rekey-test-none", std::path::Path::new("Nope")).is_none()
        );
    }

    #[tokio::test]
    async fn delete_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doomed.txt"), "goodbye").unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        let items = storages[0].list_objects(None).await.unwrap();
        let obj = &items[0];

        storages[0].delete(obj.handle).await.unwrap();
        assert!(!dir.path().join("doomed.txt").exists());
    }

    #[tokio::test]
    async fn create_folder() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        storages[0].create_folder(None, "NewFolder").await.unwrap();

        assert!(dir.path().join("NewFolder").is_dir());
    }

    #[tokio::test]
    async fn rename_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old_name.txt"), "content").unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        let items = storages[0].list_objects(None).await.unwrap();
        let obj = &items[0];

        storages[0]
            .rename(obj.handle, "new_name.txt")
            .await
            .unwrap();

        assert!(!dir.path().join("old_name.txt").exists());
        assert!(dir.path().join("new_name.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new_name.txt")).unwrap(),
            "content"
        );
    }

    #[tokio::test]
    async fn nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        std::fs::write(dir.path().join("a/b/c/deep.txt"), "deep").unwrap();
        std::fs::write(dir.path().join("a/top.txt"), "top").unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        // List root
        let root_items = storages[0].list_objects(None).await.unwrap();
        assert_eq!(root_items.len(), 1); // Only "a"
        assert_eq!(root_items[0].filename, "a");
        assert!(root_items[0].is_folder());
    }

    #[tokio::test]
    async fn read_only_storage_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "data").unwrap();

        let config = test_config_readonly(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        // Verify read-only access capability is reported
        assert!(!storages[0].info().is_writable);

        // Upload should fail
        let info = crate::mtp::NewObjectInfo::file("new.txt", 4);
        let result = storages[0].upload(None, info, bytes_stream(b"data")).await;
        assert!(result.is_err());

        // Delete should fail
        let items = storages[0].list_objects(None).await.unwrap();
        let result = storages[0].delete(items[0].handle).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn multiple_storages() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(dir1.path().join("file1.txt"), "storage1").unwrap();
        std::fs::write(dir2.path().join("file2.txt"), "storage2").unwrap();

        let config = test_config_multi(&[dir1.path(), dir2.path()]);
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        assert_eq!(storages.len(), 2);

        let items1 = storages[0].list_objects(None).await.unwrap();
        assert_eq!(items1.len(), 1);
        assert_eq!(items1[0].filename, "file1.txt");

        let items2 = storages[1].list_objects(None).await.unwrap();
        assert_eq!(items2.len(), 1);
        assert_eq!(items2[0].filename, "file2.txt");
    }

    #[tokio::test]
    async fn free_space_reflects_content() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let free_before = storages[0].info().free_space;

        // Upload a file
        let info = crate::mtp::NewObjectInfo::file("big.bin", 1000);
        let data = vec![0u8; 1000];
        storages[0]
            .upload(None, info, bytes_stream(&data))
            .await
            .unwrap();

        // Re-fetch storage info
        let storages2 = device.storages().await.unwrap();
        let free_after = storages2[0].info().free_space;

        assert!(free_after < free_before);
        assert_eq!(free_before - free_after, 1000);
    }

    #[tokio::test]
    async fn events_generated_on_upload() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        let info = crate::mtp::NewObjectInfo::file("event_test.txt", 5);
        storages[0]
            .upload(None, info, bytes_stream(b"hello"))
            .await
            .unwrap();

        // Events should be available (ObjectAdded + StorageInfoChanged)
        use tokio::time::{timeout, Duration};
        let event = timeout(Duration::from_millis(100), device.next_event()).await;
        assert!(event.is_ok());
    }

    #[tokio::test]
    async fn events_generated_on_delete() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("to_delete.txt"), "bye").unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        let items = storages[0].list_objects(None).await.unwrap();
        storages[0].delete(items[0].handle).await.unwrap();

        // Should have ObjectRemoved + StorageInfoChanged events
        use tokio::time::{timeout, Duration};
        let event = timeout(Duration::from_millis(100), device.next_event()).await;
        assert!(event.is_ok());
    }

    #[tokio::test]
    async fn no_events_returns_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();

        // No operations performed, so no events
        let result = device.next_event().await;
        assert!(matches!(result, Err(crate::mtp::Error::Timeout)));
    }

    #[tokio::test]
    async fn copy_object() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("original.txt"), "copy me").unwrap();
        std::fs::create_dir(dir.path().join("dest")).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        let items = storages[0].list_objects(None).await.unwrap();
        let original = items.iter().find(|i| i.filename == "original.txt").unwrap();
        let dest = items.iter().find(|i| i.filename == "dest").unwrap();

        storages[0]
            .copy_object(original.handle, dest.handle, None)
            .await
            .unwrap();

        // Both should exist
        assert!(dir.path().join("original.txt").exists());
        assert!(dir.path().join("dest/original.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dest/original.txt")).unwrap(),
            "copy me"
        );
    }

    #[tokio::test]
    async fn path_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        // Try to upload a file with ".." in the name
        let info = crate::mtp::NewObjectInfo::file("../escape.txt", 6);
        let result = storages[0]
            .upload(None, info, bytes_stream(b"escape"))
            .await;
        assert!(result.is_err(), "path traversal upload should be rejected");

        // Verify the file was NOT created outside the backing dir
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    #[tokio::test]
    async fn move_object() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("moveme.txt"), "move me").unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        let items = storages[0].list_objects(None).await.unwrap();
        let moveme = items.iter().find(|i| i.filename == "moveme.txt").unwrap();
        let target = items.iter().find(|i| i.filename == "target").unwrap();

        storages[0]
            .move_object(moveme.handle, target.handle, None)
            .await
            .unwrap();

        assert!(!dir.path().join("moveme.txt").exists());
        assert!(dir.path().join("target/moveme.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("target/moveme.txt")).unwrap(),
            "move me"
        );

        // Should emit ObjectInfoChanged + StorageInfoChanged events
        use tokio::time::{timeout, Duration};
        let event = timeout(Duration::from_millis(100), device.next_event())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            crate::mtp::DeviceEvent::ObjectInfoChanged { .. }
        ));
        let event = timeout(Duration::from_millis(100), device.next_event())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            crate::mtp::DeviceEvent::StorageInfoChanged { .. }
        ));
    }

    /// Helper: poll for an event, retrying on Timeout up to the deadline.
    async fn poll_event_with_retry(
        device: &MtpDevice,
        timeout_duration: std::time::Duration,
    ) -> Option<crate::mtp::DeviceEvent> {
        let deadline = tokio::time::Instant::now() + timeout_duration;
        loop {
            match device.next_event().await {
                Ok(event) => return Some(event),
                Err(crate::mtp::Error::Timeout) => {
                    if tokio::time::Instant::now() >= deadline {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
    }

    #[tokio::test]
    async fn fs_watcher_detects_file_creation() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize the backing dir to avoid macOS /var vs /private/var mismatches
        let backing_dir = dir.path().canonicalize().unwrap();
        let config = VirtualDeviceConfig {
            serial: "test-fswatch".into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: backing_dir.clone(),
                read_only: false,
            }],
            // This test is about the watcher, so state it rather than inherit it.
            watch_backing_dirs: true,
            ..Default::default()
        };

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();

        // Write a file directly to the backing dir (bypassing MTP)
        std::fs::write(backing_dir.join("external.txt"), "hello from outside").unwrap();

        // Poll for events: the watcher should detect the file creation.
        let event = poll_event_with_retry(&device, Duration::from_secs(5)).await;
        assert!(
            event.is_some(),
            "expected event from fs watcher, got nothing"
        );
        let event = event.unwrap();
        assert!(
            matches!(event, crate::mtp::DeviceEvent::ObjectAdded { .. }),
            "expected ObjectAdded, got {:?}",
            event
        );
    }

    /// Helper: create a virtual device with a pre-existing subdirectory and
    /// return (device, backing_dir, tempdir-guard).
    async fn virtual_device_with_subdirectory(
        serial: &str,
    ) -> (MtpDevice, std::path::PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let backing_dir = dir.path().canonicalize().unwrap();
        std::fs::create_dir(backing_dir.join("Music")).unwrap();

        let config = VirtualDeviceConfig {
            serial: serial.into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: backing_dir.clone(),
                read_only: false,
            }],
            // This test is about the watcher, so state it rather than inherit it.
            watch_backing_dirs: true,
            ..Default::default()
        };

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();

        // Drain any startup events (macOS FSEvents may report the watched dir).
        while poll_event_with_retry(&device, Duration::from_millis(500))
            .await
            .is_some()
        {}

        (device, backing_dir, dir)
    }

    #[tokio::test]
    async fn fs_watcher_detects_file_creation_in_subdirectory() {
        let (device, backing_dir, _dir) =
            virtual_device_with_subdirectory("test-fswatch-subdir-create").await;

        std::fs::write(backing_dir.join("Music/song.mp3"), "fake mp3 data").unwrap();

        let event = poll_event_with_retry(&device, Duration::from_secs(5)).await;
        assert!(
            event.is_some(),
            "expected event from fs watcher for file in subdirectory, got nothing"
        );
        assert!(
            matches!(event.unwrap(), crate::mtp::DeviceEvent::ObjectAdded { .. }),
            "expected ObjectAdded"
        );
    }

    #[tokio::test]
    async fn fs_watcher_detects_file_rename_in_subdirectory() {
        let (device, backing_dir, _dir) =
            virtual_device_with_subdirectory("test-fswatch-subdir-rename").await;

        // Create a file and drain its events.
        std::fs::write(backing_dir.join("Music/song.mp3"), "fake mp3 data").unwrap();
        while poll_event_with_retry(&device, Duration::from_secs(5))
            .await
            .is_some()
        {}

        // Rename the file within the subdirectory.
        std::fs::rename(
            backing_dir.join("Music/song.mp3"),
            backing_dir.join("Music/track.mp3"),
        )
        .unwrap();

        // A rename should produce an ObjectRemoved (old name) and ObjectAdded (new name).
        let mut events = Vec::new();
        while let Some(event) = poll_event_with_retry(&device, Duration::from_secs(5)).await {
            events.push(event);
            let has_removed = events
                .iter()
                .any(|e| matches!(e, crate::mtp::DeviceEvent::ObjectRemoved { .. }));
            let has_added = events
                .iter()
                .any(|e| matches!(e, crate::mtp::DeviceEvent::ObjectAdded { .. }));
            if has_removed && has_added {
                break;
            }
        }
        assert!(
            events
                .iter()
                .any(|e| matches!(e, crate::mtp::DeviceEvent::ObjectRemoved { .. })),
            "expected ObjectRemoved for rename source, got {:?}",
            events
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, crate::mtp::DeviceEvent::ObjectAdded { .. })),
            "expected ObjectAdded for rename target, got {:?}",
            events
        );
    }

    #[tokio::test]
    async fn fs_watcher_detects_file_removal_in_subdirectory() {
        let (device, backing_dir, _dir) =
            virtual_device_with_subdirectory("test-fswatch-subdir-remove").await;

        // Create a file and drain its events.
        std::fs::write(backing_dir.join("Music/song.mp3"), "fake mp3 data").unwrap();
        while poll_event_with_retry(&device, Duration::from_secs(5))
            .await
            .is_some()
        {}

        // Delete the file.
        std::fs::remove_file(backing_dir.join("Music/song.mp3")).unwrap();

        let mut events = Vec::new();
        while let Some(event) = poll_event_with_retry(&device, Duration::from_secs(5)).await {
            events.push(event);
            if events
                .iter()
                .any(|e| matches!(e, crate::mtp::DeviceEvent::ObjectRemoved { .. }))
            {
                break;
            }
        }
        assert!(
            events
                .iter()
                .any(|e| matches!(e, crate::mtp::DeviceEvent::ObjectRemoved { .. })),
            "expected ObjectRemoved for file in subdirectory, got {:?}",
            events
        );
    }

    #[tokio::test]
    async fn fs_watcher_detects_file_removal() {
        let dir = tempfile::tempdir().unwrap();
        let backing_dir = dir.path().canonicalize().unwrap();

        let config = VirtualDeviceConfig {
            serial: "test-fswatch-rm".into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: backing_dir.clone(),
                read_only: false,
            }],
            // This test is about the watcher, so state it rather than inherit it.
            watch_backing_dirs: true,
            ..Default::default()
        };

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();

        // Create the file AFTER the watcher is running, so we get a clean event sequence
        std::fs::write(backing_dir.join("will_be_removed.txt"), "bye").unwrap();

        // Drain events until no more arrive (consume the ObjectAdded from creation)
        while poll_event_with_retry(&device, Duration::from_millis(500))
            .await
            .is_some()
        {}

        // Now remove the file directly (bypassing MTP)
        std::fs::remove_file(backing_dir.join("will_be_removed.txt")).unwrap();

        // Collect all events and look for ObjectRemoved
        let mut events = Vec::new();
        while let Some(event) = poll_event_with_retry(&device, Duration::from_secs(5)).await {
            events.push(event);
            // Stop after we find what we need or have collected enough
            if events.len() >= 10 {
                break;
            }
            if events
                .iter()
                .any(|e| matches!(e, crate::mtp::DeviceEvent::ObjectRemoved { .. }))
            {
                break;
            }
        }

        assert!(
            events
                .iter()
                .any(|e| matches!(e, crate::mtp::DeviceEvent::ObjectRemoved { .. })),
            "expected ObjectRemoved among events, got {:?}",
            events
        );
    }

    #[tokio::test]
    async fn fs_watcher_dedup_suppresses_mtp_events() {
        let dir = tempfile::tempdir().unwrap();
        let backing_dir = dir.path().canonicalize().unwrap();
        let config = VirtualDeviceConfig {
            serial: "test-fswatch-dedup".into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: backing_dir.clone(),
                read_only: false,
            }],
            // This test is about the watcher, so state it rather than inherit it.
            watch_backing_dirs: true,
            ..Default::default()
        };

        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        // Upload via MTP: should produce exactly the MTP-generated events
        let info = crate::mtp::NewObjectInfo::file("dedup_test.txt", 5);
        storages[0]
            .upload(None, info, bytes_stream(b"hello"))
            .await
            .unwrap();

        // Drain all events with a generous window for the watcher to fire.
        // MTP upload produces 1 ObjectAdded + 1 StorageInfoChanged.
        // The watcher sees the file creation but finds the handle already exists
        // in state.objects (inserted by the MTP handler under the mutex), so it
        // skips the event, so no duplicate ObjectAdded.
        // We count ObjectAdded specifically because some platforms (Linux inotify)
        // may generate additional filesystem events (StorageInfoChanged etc.).
        let mut object_added_count = 0;
        let mut total_events = 0;
        while let Some(event) = poll_event_with_retry(&device, Duration::from_millis(500)).await {
            if matches!(event, crate::mtp::DeviceEvent::ObjectAdded { .. }) {
                object_added_count += 1;
            }
            total_events += 1;
            if total_events > 10 {
                break;
            }
        }

        // Exactly 1 ObjectAdded from the MTP handler. The watcher's dedup
        // suppresses duplicates.
        assert_eq!(
            object_added_count, 1,
            "expected exactly 1 ObjectAdded event, got {} (dedup may have failed)",
            object_added_count
        );
    }

    fn test_config_with_serial(dir: &std::path::Path, serial: &str) -> VirtualDeviceConfig {
        VirtualDeviceConfig {
            manufacturer: "TestCorp".into(),
            model: "Virtual Phone".into(),
            serial: serial.into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: dir.to_path_buf(),
                read_only: false,
            }],
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn rescan_detects_external_changes() {
        let dir = tempfile::tempdir().unwrap();
        // Create initial files before opening the device.
        std::fs::write(dir.path().join("existing.txt"), "hello").unwrap();
        std::fs::create_dir(dir.path().join("Photos")).unwrap();
        std::fs::write(dir.path().join("Photos/pic.jpg"), "jpeg data").unwrap();

        let config = test_config_with_serial(dir.path(), "rescan-test-001");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        // List all objects to populate the in-memory tree.
        let root_items = storages[0].list_objects(None).await.unwrap();
        assert_eq!(root_items.len(), 2); // existing.txt + Photos
        let photos = root_items.iter().find(|i| i.filename == "Photos").unwrap();
        let _sub_items = storages[0].list_objects(Some(photos.handle)).await.unwrap();

        // Drain any events from listing.
        while device.next_event().await.is_ok() {}

        // --- Externally modify the backing dir (bypassing MTP) ---
        // Delete existing.txt
        std::fs::remove_file(dir.path().join("existing.txt")).unwrap();
        // Create a new file
        std::fs::write(dir.path().join("new_file.txt"), "I'm new").unwrap();
        // Delete the file inside Photos
        std::fs::remove_file(dir.path().join("Photos/pic.jpg")).unwrap();

        // Rescan via the public API.
        let summary = crate::rescan_virtual_device("rescan-test-001").unwrap();
        assert_eq!(
            summary.removed, 2,
            "should remove existing.txt and Photos/pic.jpg"
        );
        assert_eq!(summary.added, 1, "should add new_file.txt");

        // Verify the object tree now matches the filesystem.
        let root_items = storages[0].list_objects(None).await.unwrap();
        let names: Vec<&str> = root_items.iter().map(|i| i.filename.as_str()).collect();
        assert!(
            names.contains(&"new_file.txt"),
            "new_file.txt should appear: {:?}",
            names
        );
        assert!(
            names.contains(&"Photos"),
            "Photos dir still exists: {:?}",
            names
        );
        assert!(
            !names.contains(&"existing.txt"),
            "existing.txt should be gone: {:?}",
            names
        );

        // Verify Photos subdirectory is now empty.
        let photos = root_items.iter().find(|i| i.filename == "Photos").unwrap();
        let sub_items = storages[0].list_objects(Some(photos.handle)).await.unwrap();
        assert!(
            sub_items.is_empty(),
            "Photos should be empty after pic.jpg was removed"
        );

        // Verify events were queued (ObjectRemoved, ObjectAdded, StorageInfoChanged).
        let mut event_types = Vec::new();
        loop {
            match device.next_event().await {
                Ok(e) => event_types.push(e),
                Err(crate::mtp::Error::Timeout) => break,
                Err(_) => break,
            }
        }
        assert!(
            event_types
                .iter()
                .any(|e| matches!(e, crate::mtp::DeviceEvent::ObjectAdded { .. })),
            "expected ObjectAdded event from rescan"
        );
        assert!(
            event_types
                .iter()
                .any(|e| matches!(e, crate::mtp::DeviceEvent::ObjectRemoved { .. })),
            "expected ObjectRemoved event from rescan"
        );
    }

    #[test]
    fn rescan_nonexistent_serial_returns_none() {
        assert!(
            crate::transport::virtual_device::registry::rescan_virtual_device("no-such-device")
                .is_none()
        );
    }

    #[tokio::test]
    async fn rescan_no_changes_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stable.txt"), "unchanged").unwrap();

        let config = test_config_with_serial(dir.path(), "rescan-test-002");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();

        // Populate the object tree.
        let _ = storages[0].list_objects(None).await.unwrap();

        let summary = crate::rescan_virtual_device("rescan-test-002").unwrap();
        assert_eq!(summary.added, 0);
        assert_eq!(summary.removed, 0);

        drop(device);
    }

    #[tokio::test]
    async fn list_objects_resolves_full_size_for_files_larger_than_4gb() {
        // Create a 5 GB sparse file on the backing filesystem. Its ObjectInfo size
        // field will saturate at u32::MAX; the real u64 size must be resolved via
        // GetObjectPropValue(ObjectSize).
        const REAL_SIZE: u64 = 5 * 1024 * 1024 * 1024;

        let dir = tempfile::tempdir().unwrap();
        let big_path = dir.path().join("movie.mkv");
        let file = std::fs::File::create(&big_path).unwrap();
        file.set_len(REAL_SIZE).unwrap();
        drop(file);

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let objects = storages[0].list_objects(None).await.unwrap();

        let movie = objects
            .iter()
            .find(|o| o.filename == "movie.mkv")
            .expect("movie.mkv not found in listing");
        assert_eq!(
            movie.size, REAL_SIZE,
            "full u64 size should be resolved for files >4 GB"
        );
    }
}
