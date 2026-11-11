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
//! use mtp_rs::virtual_device::{VirtualDeviceConfig, VirtualStorageConfig};
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
mod prop_list;
pub mod registry;
mod state;
mod watcher;

use crate::ptp::{unpack_u16, unpack_u32};
use crate::transport::Transport;
use async_trait::async_trait;
use config::VirtualDeviceConfig;
use state::{PendingCommand, VirtualDeviceState};
pub use state::{RescanSummary, DROPPED_PATHS_CAP};
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Set when `GetDeviceInfo` shows Android's `MtpServer`, as the virtual device's does. See
    /// [`Transport::set_drain_on_cancel`].
    drain_on_cancel: AtomicBool,
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
            drain_on_cancel: AtomicBool::new(false),
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
/// `No_Valid_ObjectInfo`: a `SendObject` with no `SendObjectInfo` in effect.
const NO_VALID_OBJECT_INFO: u16 = 0x2015;

#[async_trait]
impl Transport for VirtualTransport {
    async fn send_bulk(&self, data: &[u8]) -> Result<(), crate::PtpError> {
        let mut state = self.state.lock().unwrap();
        if state.wedged {
            // A wedged phone reads nothing we send as a command again (see `wedged`).
            return Ok(());
        }

        // While a data phase is open, Android's `MtpServer` reads the bulk OUT pipe as data, so
        // anything that isn't a data container (a zero-length packet, or the next command) ends
        // it short. A camera's data phase ends with CLASS_CANCEL instead (`cancel_transfer`).
        let is_data = data.len() >= 12 && unpack_u16(&data[4..6])? == CONTAINER_TYPE_DATA;
        if state.config.android && state.pending_command.is_some() && !is_data {
            handlers::end_data_phase_short(&mut state);
            return Ok(());
        }
        if data.is_empty() {
            // A zero-length packet with no data phase open: nothing to read.
            return Ok(());
        }
        if data.len() < 12 {
            return Err(crate::PtpError::invalid_data("container too small"));
        }

        let _length = unpack_u32(&data[0..4])?;
        let container_type = unpack_u16(&data[4..6])?;
        let code = unpack_u16(&data[6..8])?;
        let tx_id = unpack_u32(&data[8..12])?;

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

                let op = crate::ptp::OperationCode::from(code);
                if state.config.android {
                    // AOSP's `MtpServer::handleRequest` forgets a pending `SendObjectInfo` on any
                    // command other than `SendObject`, so the object it described never gets its
                    // data. (AOSP keeps a cache entry with no file; here the empty placeholder
                    // `SendObjectInfo` made stays.)
                    if op != crate::ptp::OperationCode::SendObject {
                        // A file's object stays listed with no file behind it: a ghost.
                        if let Some(send) = state.pending_send.take() {
                            if !send.is_folder {
                                state.ghosts.insert(send.assigned_handle.0);
                            }
                        }
                    } else if state.pending_send.is_none() {
                        // `doSendObject` answers `NoValidObjectInfo` straight away and never reads
                        // the data phase. The data container that follows reaches the device as
                        // the next "command" (see the data arm below).
                        state.response_queue.push_back(builders::build_response(
                            NO_VALID_OBJECT_INFO,
                            tx_id,
                            &[],
                        ));
                        state.refused_data_unread = true;
                        return Ok(());
                    }
                }
                // AOSP checks a `SendPartialObject` before reading its data, and the refused data
                // then reaches the device as commands, which wedged a Pixel (#62).
                if op == crate::ptp::OperationCode::SendPartialObject
                    && state.edit_objects_enabled()
                    && !handlers::partial_write_accepted(&state, &params)
                {
                    state.response_queue.push_back(builders::build_response(
                        handlers::GENERAL_ERROR,
                        tx_id,
                        &[],
                    ));
                    state.refused_data_unread = true;
                    return Ok(());
                }
                // Check if this operation expects a data phase from the host.
                // If so, don't dispatch yet -- store the command and wait for data.
                if matches!(
                    op,
                    crate::ptp::OperationCode::SendObjectInfo
                        | crate::ptp::OperationCode::SendObject
                        | crate::ptp::OperationCode::SetObjectPropValue
                ) || (op == crate::ptp::OperationCode::SendPartialObject
                    && state.edit_objects_enabled())
                {
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
                    // AOSP's run loop reads a stray data container as a request, and
                    // `handleRequest` drops it without an answer ("wrong container type"). The
                    // payload of a data phase the device refused unread is another matter: the
                    // phone reads it as a stream of garbage commands and stops answering.
                    None if state.config.android => {
                        if std::mem::take(&mut state.refused_data_unread) && data.len() > 12 {
                            state.wedged = true;
                        }
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

    async fn receive_bulk(&self, max_size: usize) -> Result<Vec<u8>, crate::PtpError> {
        let mut state = self.state.lock().unwrap();
        // A wedged device still sends what it had queued (its answer to the refused command),
        // then nothing.
        if state.wedged && state.response_queue.is_empty() {
            return Err(crate::PtpError::Timeout);
        }
        match state.response_queue.pop_front() {
            Some(data) if max_size > 0 && data.len() > max_size => {
                // A USB bulk read returns at most `max_size` bytes, so a container longer than
                // that arrives over several reads, as on hardware (#63). The rest goes back to
                // the front of the queue in `max_size` pieces, cut in one pass so a big object
                // isn't copied once per read.
                let mut pieces = data.chunks(max_size).map(<[u8]>::to_vec);
                let first = pieces.next().expect("data is longer than max_size");
                for piece in pieces.rev() {
                    state.response_queue.push_front(piece);
                }
                Ok(first)
            }
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
        // A CLASS_CANCEL ends an open data phase. On Android no CLASS_CANCEL goes out (#54), only
        // a drain of bulk IN, so the device keeps reading bulk OUT as data.
        if !self.drain_on_cancel.load(Ordering::SeqCst) {
            state.pending_command = None;
        }
        state.response_queue.clear();
        // Model the Samsung cancel wedge (#18): when armed via
        // `force_cancel_wedge`, report `DeviceReset` (one-shot), as the real USB
        // transport does after detecting the wedge and resetting the device.
        if std::mem::take(&mut state.pending_cancel_wedge) {
            return Err(crate::PtpError::DeviceReset);
        }
        Ok(())
    }

    fn set_drain_on_cancel(&self, enabled: bool) {
        self.drain_on_cancel.store(enabled, Ordering::SeqCst);
    }

    async fn end_send_phase(&self) -> Result<(), crate::PtpError> {
        // Same rule as the USB transport: a zero-length packet on Android, nothing elsewhere.
        if self.drain_on_cancel.load(Ordering::SeqCst) {
            self.send_bulk(&[]).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod paced_upload_tests;

#[cfg(test)]
mod tests {
    use super::config::{VirtualDeviceConfig, VirtualStorageConfig};
    use crate::mtp::{ByteRange, DownloadPacing, MtpDevice, ObjectFormat};
    use crate::ptp::OperationCode;
    use crate::Error;
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

    /// `test_config` with `android: false`: a PTP camera, which the host cancels with
    /// CLASS_CANCEL instead of draining.
    fn camera_config(dir: &std::path::Path) -> VirtualDeviceConfig {
        VirtualDeviceConfig {
            android: false,
            ..test_config(dir)
        }
    }

    #[tokio::test]
    async fn the_android_setting_picks_how_the_host_cancels() {
        use crate::ptp::PtpSession;
        use std::sync::atomic::Ordering;
        use std::sync::Arc;

        for android in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let config = VirtualDeviceConfig {
                android,
                ..test_config_with_serial(dir.path(), &format!("cancel-mode-{android}"))
            };
            let transport = Arc::new(super::VirtualTransport::new(config));
            let dyn_transport: Arc<dyn crate::transport::Transport> = transport.clone();
            let session = PtpSession::open(dyn_transport, 1).await.unwrap();
            let info = session.get_device_info().await.unwrap();
            assert_eq!(info.runs_android_mtp_server(), android);
            // Android gets a drain, a camera gets CLASS_CANCEL.
            assert_eq!(transport.drain_on_cancel.load(Ordering::SeqCst), android);
        }
    }

    #[tokio::test]
    async fn cancel_mid_partial_stream_leaves_session_usable() {
        let dir = tempfile::tempdir().unwrap();
        cancel_mid_partial_stream_then_resume(test_config(dir.path()), dir.path()).await;
    }

    #[tokio::test]
    async fn cancel_mid_partial_stream_on_a_camera_leaves_session_usable() {
        let dir = tempfile::tempdir().unwrap();
        cancel_mid_partial_stream_then_resume(camera_config(dir.path()), dir.path()).await;
    }

    async fn cancel_mid_partial_stream_then_resume(
        config: VirtualDeviceConfig,
        dir: &std::path::Path,
    ) {
        use crate::mtp::DEFAULT_CANCEL_TIMEOUT;

        let content: Vec<u8> = (0..8000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.join("data.bin"), &content).unwrap();

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
        assert!(crate::virtual_device::force_object_info_error(
            serial, victim, None
        ));

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
        assert!(crate::virtual_device::clear_object_info_errors(serial));
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
            assert!(crate::virtual_device::force_object_info_error(
                serial, obj.handle, None
            ));
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
        assert!(crate::virtual_device::force_cancel_wedge(serial));

        let mut dl = storages[0]
            .download_with_pacing(obj.handle, ByteRange::Full, DownloadPacing::HeldOpen)
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
            .download_with_pacing(obj.handle, ByteRange::Full, DownloadPacing::HeldOpen)
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
        assert!(crate::virtual_device::force_operation_wedge(serial));

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

        assert!(crate::virtual_device::force_operation_wedge(serial));

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
        assert!(!crate::virtual_device::force_operation_wedge(
            "no-such-device"
        ));
    }

    // ---- Paced downloads (windows that free the session between them, #60) ----

    /// A download in windows of `window` bytes.
    async fn windowed(
        storage: &crate::mtp::Storage,
        handle: crate::mtp::ObjectHandle,
        range: ByteRange,
        window: u32,
    ) -> crate::mtp::FileDownload {
        storage
            .download_with_pacing(handle, range, DownloadPacing::FixedWindow(window))
            .await
            .unwrap()
    }

    /// Read chunks until one window ends (the session is free again), returning that window's
    /// bytes.
    async fn next_window(dl: &mut crate::mtp::FileDownload) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(chunk) = dl.next_chunk().await {
            out.extend_from_slice(&chunk.expect("chunk should not error"));
            if !dl.holds_session() {
                return Some(out);
            }
        }
        (!out.is_empty()).then_some(out)
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

        let dl = windowed(&storages[0], obj.handle, ByteRange::Full, 1024).await;
        assert_eq!(dl.size(), content.len() as u64, "size() is the full object");
        let got = collect_download(dl).await;
        assert_eq!(got, content, "windowed reassembly must equal the file");
        assert_eq!(got, storages[0].download_to_vec(obj.handle).await.unwrap());
    }

    #[tokio::test]
    async fn every_pacing_reads_the_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let device = MtpDevice::builder()
            .open_virtual(test_config(dir.path()))
            .await
            .unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // The default (adaptive) download starts with an 8 MiB window, so this file comes
        // back in one window.
        let default = storages[0]
            .download(obj.handle, ByteRange::Full)
            .await
            .unwrap();
        assert_eq!(collect_download(default).await, content);
        for pacing in [
            DownloadPacing::HeldOpen,
            DownloadPacing::FixedWindow(4096),
            DownloadPacing::FixedWindow(0),
            DownloadPacing::MaxCancelDelay(Duration::ZERO),
        ] {
            let dl = storages[0]
                .download_with_pacing(obj.handle, ByteRange::From(1000), pacing)
                .await
                .unwrap();
            assert_eq!(collect_download(dl).await, content[1000..], "{pacing:?}");
        }
    }

    #[tokio::test]
    async fn windowed_download_stops_at_the_end_of_a_range() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let device = MtpDevice::builder()
            .open_virtual(test_config(dir.path()))
            .await
            .unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        let range = ByteRange::Range {
            offset: 700,
            len: 1500,
        };
        let dl = windowed(&storages[0], obj.handle, range, 512).await;
        assert_eq!(collect_download(dl).await, content[700..2200]);

        // A range that runs past the end stops at the end.
        let past_end = ByteRange::Range {
            offset: 4000,
            len: 9000,
        };
        let dl = windowed(&storages[0], obj.handle, past_end, 512).await;
        assert_eq!(collect_download(dl).await, content[4000..]);
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

        let dl = windowed(&storages[0], obj.handle, ByteRange::Full, 1024).await;
        assert_eq!(dl.size(), content.len() as u64);
        assert_eq!(collect_download(dl).await, content);

        // A held-open ranged download from an offset uses the 32-bit op too.
        let offset = 1234u64;
        let ranged = storages[0]
            .download_with_pacing(
                obj.handle,
                ByteRange::From(offset),
                DownloadPacing::HeldOpen,
            )
            .await
            .unwrap();
        assert_eq!(ranged.size(), content.len() as u64);
        assert_eq!(collect_download(ranged).await, content[offset as usize..]);

        // And a plain read_range with an explicit length.
        let mid = storages[0].read_range(obj.handle, 100, 50).await.unwrap();
        assert_eq!(mid, content[100..150]);
    }

    #[tokio::test]
    async fn whole_object_download_is_the_only_read_on_a_no_partial_device() {
        // libhaze (Sphaira on the Nintendo Switch) handles neither partial-read
        // operation, so a host can only ask for whole objects. Consumers need this
        // shape to test their own fallback: without it, every virtual device serves
        // ranged reads and the fallback path never runs.
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = VirtualDeviceConfig {
            serial: "test-no-partial".into(),
            supports_partial_object: false,
            supports_partial_object_64: false,
            ..test_config(dir.path())
        };
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        assert!(!device.capabilities().supports_partial_download);
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // The default (windowed) pacing falls back to one GetObject, which needs no
        // partial-read op.
        let full = storages[0]
            .download(obj.handle, ByteRange::Full)
            .await
            .unwrap();
        assert!(full.holds_session(), "a held-open read starts at the call");
        assert_eq!(full.size(), content.len() as u64);
        assert_eq!(collect_download(full).await, content);

        // Every ranged read is refused before it reaches the wire, at the call.
        assert!(matches!(
            storages[0].read_range(obj.handle, 100, 50).await,
            Err(Error::Unsupported)
        ));
        assert!(matches!(
            storages[0].download(obj.handle, ByteRange::From(100)).await,
            Err(Error::Unsupported)
        ));

        // The session survives all of that: a refusal must not wedge the device.
        assert_eq!(storages[0].list_objects(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn held_open_download_arrives_in_usb_sized_chunks() {
        // Real USB reads a held-open GetObject 64 KiB at a time, so `next_chunk` never hands
        // over more than that. A consumer that bounds memory per call has to be able to see
        // the same on the virtual device (#63), not the whole object as one chunk.
        const USB_READ: usize = 64 * 1024;
        let dir = tempfile::tempdir().unwrap();
        // A multi-MiB object, and one whose data container (12-byte header + payload) is
        // exactly two reads long.
        let sizes = [3 * 1024 * 1024 + 123, 2 * USB_READ - 12];
        for (i, size) in sizes.iter().enumerate() {
            let content: Vec<u8> = (0..*size).map(|b| (b % 251) as u8).collect();
            std::fs::write(dir.path().join(format!("big{i}.bin")), &content).unwrap();
        }

        let config = VirtualDeviceConfig {
            serial: "test-held-open-chunks".into(),
            supports_partial_object: false,
            supports_partial_object_64: false,
            ..test_config(dir.path())
        };
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let mut objects = storages[0].list_objects(None).await.unwrap();
        objects.sort_by(|a, b| a.filename.cmp(&b.filename));

        for (obj, size) in objects.iter().zip(sizes) {
            let content = std::fs::read(dir.path().join(&obj.filename)).unwrap();
            let mut dl = storages[0]
                .download(obj.handle, ByteRange::Full)
                .await
                .unwrap();
            assert!(
                dl.holds_session(),
                "a no-partial device reads with a held-open GetObject"
            );

            let mut got = Vec::with_capacity(size);
            let mut chunks = 0;
            while let Some(chunk) = dl.next_chunk().await {
                let chunk = chunk.expect("chunk should not error");
                assert!(
                    chunk.len() <= USB_READ,
                    "{}: a {}-byte chunk is larger than one USB read",
                    obj.filename,
                    chunk.len()
                );
                got.extend_from_slice(&chunk);
                chunks += 1;
            }
            assert!(chunks > 1, "{}: came as one chunk", obj.filename);
            assert_eq!(got, content, "{}: reassembles byte-exact", obj.filename);
        }

        // The data phase and its response were read to the end: the session still works.
        assert_eq!(storages[0].list_objects(None).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_unadvertised_partial_op_is_refused_on_the_wire() {
        // The capability flag and the dispatch have to agree. A device that leaves an
        // operation out of OperationsSupported and then serves it anyway would let a
        // consumer bug (calling the op regardless of the capability) pass in tests and
        // fail on hardware.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"0123456789").unwrap();

        for (partial_32, partial_64, op) in [
            (false, true, OperationCode::GetPartialObject),
            (true, false, OperationCode::GetPartialObject64),
        ] {
            let config = VirtualDeviceConfig {
                serial: format!("test-refuses-{:04x}", u16::from(op)),
                supports_partial_object: partial_32,
                supports_partial_object_64: partial_64,
                ..test_config(dir.path())
            };
            let mut state = super::state::VirtualDeviceState::new(config);
            state.session_open = true;
            let params = [1u32, 0, 4, 0];
            super::handlers::dispatch(&mut state, u16::from(op), 7, &params, None);

            let response = state.response_queue.pop_back().expect("a response");
            assert_eq!(
                u16::from_le_bytes([response[6], response[7]]),
                0x2005,
                "{op:?} was advertised as unsupported but still answered"
            );
        }
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
            let dl = windowed(&storages[0], obj.handle, ByteRange::From(offset), 512).await;
            assert_eq!(
                dl.size(),
                content.len() as u64,
                "size() must report the full object size at offset {offset}"
            );
            assert_eq!(dl.offset(), offset, "offset() starts at the resume point");
            let got = collect_download(dl).await;
            assert_eq!(
                got,
                content[offset as usize..],
                "tail bytes wrong for offset {offset}"
            );
        }
    }

    #[tokio::test]
    async fn windowed_download_at_size_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"exactly this many bytes".to_vec();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        let mut dl = windowed(
            &storages[0],
            obj.handle,
            ByteRange::From(content.len() as u64),
            64,
        )
        .await;
        // offset == size: a clean None, no read issued.
        assert!(dl.next_chunk().await.is_none());

        // The session is immediately usable, with the download still in scope.
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

        // `FileDownload` isn't `Debug`, so match the result directly.
        for pacing in [DownloadPacing::default(), DownloadPacing::HeldOpen] {
            match storages[0]
                .download_with_pacing(
                    obj.handle,
                    ByteRange::From(content.len() as u64 + 1),
                    pacing,
                )
                .await
            {
                Err(crate::mtp::Error::InvalidData { .. }) => {}
                Err(other) => panic!("expected InvalidData for offset past size, got {other:?}"),
                Ok(_) => panic!("offset past size must error, not return a download"),
            }
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

        let mut dl = windowed(&storages[0], obj.handle, ByteRange::Full, 256).await;
        let mut sizes = Vec::new();
        while let Some(window) = next_window(&mut dl).await {
            sizes.push(window.len());
        }
        assert_eq!(sizes, vec![256, 256, 256, 232], "clean short final window");
    }

    #[tokio::test]
    async fn windowed_download_empty_file_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.bin"), b"").unwrap();

        let config = test_config_with_serial(dir.path(), "windowed-empty-001");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();
        assert_eq!(obj.size, 0);

        // Arm a stall on any partial read so we can prove NO read is issued for an
        // empty file: a read would consume this cap.
        assert!(crate::virtual_device::force_partial_read_caps(
            "windowed-empty-001",
            vec![0]
        ));

        let mut dl = windowed(&storages[0], obj.handle, ByteRange::Full, 64).await;
        assert!(dl.next_chunk().await.is_none(), "empty file: a clean None");

        // Prove no read was issued: the armed cap is still pending. Issue one real
        // partial read now: it pops the still-armed cap and comes back empty,
        // which only holds if the download left the cap untouched.
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

        let mut dl = windowed(&storages[0], obj.handle, ByteRange::Full, 512).await;
        // First window reads fine.
        let first = next_window(&mut dl).await.expect("first window");
        assert_eq!(first.len(), 512);

        // Force the NEXT read to return 0 bytes mid-file (a device stall).
        assert!(crate::virtual_device::force_partial_read_caps(
            "windowed-stall-001",
            vec![0]
        ));
        match dl.next_chunk().await {
            Some(Err(crate::mtp::Error::InvalidData { .. })) => {}
            Some(Err(other)) => panic!("expected InvalidData stall error, got {other:?}"),
            Some(Ok(b)) => panic!("expected a stall error, got {} bytes", b.len()),
            None => panic!("a 0-byte read mid-file must be an error, NOT a clean EOF"),
        }
        assert!(dl.next_chunk().await.is_none(), "the download is over");
        assert_eq!(storages[0].list_objects(None).await.unwrap().len(), 1);
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
        assert!(crate::virtual_device::force_partial_read_caps(
            "windowed-short-001",
            vec![100, 50]
        ));

        // A short window stays open until its end arrives, which the next call reads before
        // starting the next window. A window this small (under one 64 KiB read) arrives as one
        // chunk, so only the forced caps leave it open.
        let mut dl = windowed(&storages[0], obj.handle, ByteRange::Full, 512).await;
        let w1 = dl.next_chunk().await.unwrap().unwrap();
        assert_eq!(w1.len(), 100, "first read clamped to 100 bytes");
        assert_eq!(dl.offset(), 100, "offset advanced by bytes returned");
        let w2 = dl.next_chunk().await.unwrap().unwrap();
        assert_eq!(w2.len(), 50, "second read clamped to 50 bytes");
        assert_eq!(dl.offset(), 150);

        // Remaining windows are full-size; the whole thing reassembles exactly.
        let mut got = w1.to_vec();
        got.extend_from_slice(&w2);
        got.extend(collect_download(dl).await);
        assert_eq!(got, content, "short mid-file reads stay byte-exact");
    }

    #[tokio::test]
    async fn windowed_download_session_free_between_windows() {
        // The headline property: nothing is held between windows, so another
        // device operation succeeds between two windows, with the download in scope.
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

        let mut dl = windowed(&storages[0], big.handle, ByteRange::Full, 512).await;
        assert!(!dl.holds_session(), "a windowed download starts lazily");

        // Read one window: the call that returns its last bytes also closes it.
        let w1 = next_window(&mut dl).await.expect("first window");
        assert_eq!(w1.len(), 512);
        assert!(!dl.holds_session());

        // Now run a full listing on the SAME session without cancelling or dropping.
        let listed = storages[0].list_objects(None).await.unwrap();
        assert_eq!(listed.len(), 2, "listing succeeds between windows");

        // The download then continues correctly from where it left off.
        let mut got = w1;
        got.extend(collect_download(dl).await);
        assert_eq!(
            got, content,
            "download resumes byte-exact after the listing"
        );
    }

    #[tokio::test]
    async fn a_listing_waiting_for_the_device_gets_in_between_windows() {
        // A listing started while a window is in flight waits for that window, then runs
        // before the next one starts (the download yields between windows).
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..64 * 1024).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("big.bin"), &content).unwrap();

        let device = MtpDevice::builder()
            .open_virtual(test_config(dir.path()))
            .await
            .unwrap();
        let storages = device.storages().await.unwrap();
        let storage = &storages[0];
        let handle = storage.list_objects(None).await.unwrap()[0].handle;

        let mut dl = windowed(storage, handle, ByteRange::Full, 4096).await;
        // Start the first window, then queue a listing behind it.
        let first = dl.next_chunk().await.unwrap().unwrap();
        let listing = storage.list_objects(None);
        let reading = async {
            let mut got = first.to_vec();
            got.extend(collect_download(dl).await);
            got
        };
        let (listed, got) = futures::join!(listing, reading);
        assert_eq!(listed.unwrap().len(), 1);
        assert_eq!(got, content);
    }

    #[tokio::test]
    async fn cancel_between_windows_sends_nothing_and_mid_window_reaches_the_device() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..8000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let serial = "paced-cancel-001";
        let device = MtpDevice::builder()
            .open_virtual(test_config_with_serial(dir.path(), serial))
            .await
            .unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // Between windows: nothing to stop, so a wedge armed on the next cancel stays unused.
        assert!(crate::virtual_device::force_cancel_wedge(serial));
        let mut dl = windowed(&storages[0], obj.handle, ByteRange::Full, 1000).await;
        next_window(&mut dl).await.unwrap();
        assert!(!dl.holds_session());
        dl.cancel(crate::mtp::DEFAULT_CANCEL_TIMEOUT)
            .await
            .expect("a cancel between windows sends nothing");
        assert!(
            dl.next_chunk().await.is_none(),
            "a cancelled download is over"
        );
        assert_eq!(dl.offset(), 1000, "offset() is where to resume");

        // Mid-window (a short read leaves the window open until its end arrives): the
        // cancel reaches the device, and the still-armed wedge fires.
        assert!(crate::virtual_device::force_partial_read_caps(
            serial,
            vec![100]
        ));
        let mut dl = windowed(&storages[0], obj.handle, ByteRange::Full, 1000).await;
        dl.next_chunk().await.unwrap().unwrap();
        assert!(dl.holds_session(), "mid-window");
        let result = dl.cancel(crate::mtp::DEFAULT_CANCEL_TIMEOUT).await;
        assert!(
            matches!(result, Err(Error::DeviceReset)),
            "expected the armed wedge, got {result:?}"
        );
        assert!(!dl.holds_session());
    }

    #[tokio::test]
    async fn dropping_a_download_mid_window_leaves_the_session_usable() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("data.bin"), &content).unwrap();

        let serial = "paced-drop-001";
        let device = MtpDevice::builder()
            .open_virtual(test_config_with_serial(dir.path(), serial))
            .await
            .unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // A short read leaves the window open, with its closing response still unread.
        assert!(crate::virtual_device::force_partial_read_caps(
            serial,
            vec![100]
        ));
        let mut dl = windowed(&storages[0], obj.handle, ByteRange::Full, 1000).await;
        dl.next_chunk().await.unwrap().unwrap();
        assert!(dl.holds_session(), "still mid-window");
        drop(dl);

        assert_eq!(storages[0].list_objects(None).await.unwrap().len(), 1);
        assert_eq!(
            storages[0].download_to_vec(obj.handle).await.unwrap(),
            content
        );
    }

    // ---- Dates (DateModified / DateCreated) ----

    /// 2024-03-15T14:30:22Z.
    const SEEDED_UNIX: i64 = 1_710_513_022;

    fn set_mtime(path: &std::path::Path, unix: i64) {
        let time = std::time::UNIX_EPOCH + Duration::from_secs(unix as u64);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    fn mtime_unix(path: &std::path::Path) -> i64 {
        let modified = std::fs::metadata(path).unwrap().modified().unwrap();
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn offset(minutes: i16) -> crate::mtp::UtcOffset {
        crate::mtp::UtcOffset::from_minutes(minutes).unwrap()
    }

    #[tokio::test]
    async fn object_info_reports_the_backing_files_modified_time_zoneless() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, b"jpeg").unwrap();
        set_mtime(&path, SEEDED_UNIX);

        let device = MtpDevice::builder()
            .open_virtual(test_config(dir.path()))
            .await
            .unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        // The default clock is UTC, written without a zone the way Android
        // writes the phone's local time.
        assert_eq!(
            obj.modified,
            crate::mtp::DateTime::new(2024, 3, 15, 14, 30, 22)
        );
    }

    #[tokio::test]
    async fn reported_dates_follow_the_configured_clock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, b"jpeg").unwrap();
        set_mtime(&path, SEEDED_UNIX);

        for include in [false, true] {
            let config = VirtualDeviceConfig {
                utc_offset: offset(120),
                dates_include_offset: include,
                ..test_config(dir.path())
            };
            let device = MtpDevice::builder().open_virtual(config).await.unwrap();
            let storages = device.storages().await.unwrap();
            let modified = storages[0].list_objects(None).await.unwrap()[0]
                .modified
                .unwrap();

            assert_eq!(
                (modified.hour, modified.minute),
                (16, 30),
                "include={include}"
            );
            assert_eq!(modified.offset, include.then(|| offset(120)));
            assert_eq!(
                modified.to_unix_seconds_with_fallback(offset(120)),
                Some(SEEDED_UNIX)
            );
        }
    }

    #[tokio::test]
    async fn windowed_download_reports_the_object_dates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, b"payload").unwrap();
        set_mtime(&path, SEEDED_UNIX);

        let device = MtpDevice::builder()
            .open_virtual(test_config(dir.path()))
            .await
            .unwrap();
        let storages = device.storages().await.unwrap();
        let obj = storages[0].list_objects(None).await.unwrap()[0].clone();

        let dl = windowed(&storages[0], obj.handle, ByteRange::Full, 4).await;
        assert_eq!(
            dl.modified(),
            crate::mtp::DateTime::new(2024, 3, 15, 14, 30, 22)
        );
        // DateCreated is the file's birth time where the host filesystem
        // records one, so compare against what ObjectInfo itself reports.
        assert_eq!(dl.created(), obj.created);
    }

    #[tokio::test]
    async fn upload_stores_the_modified_date_it_was_sent() {
        let utc = crate::mtp::DateTime::from_unix_seconds(SEEDED_UNIX, crate::mtp::UtcOffset::UTC)
            .unwrap();
        let zoneless = crate::mtp::DateTime {
            offset: None,
            ..utc
        };
        let cases = [
            // An explicit offset is an instant, whatever the device's clock says.
            ("utc.txt", utc, SEEDED_UNIX),
            (
                "ist.txt",
                utc.with_offset(offset(330)),
                SEEDED_UNIX - 330 * 60,
            ),
            // A zoneless date is device-local time: here UTC+1.
            ("local.txt", zoneless, SEEDED_UNIX - 3600),
        ];

        let dir = tempfile::tempdir().unwrap();
        let config = VirtualDeviceConfig {
            utc_offset: offset(60),
            ..test_config(dir.path())
        };
        let device = MtpDevice::builder()
            .use_windowed_uploads(false)
            .open_virtual(config)
            .await
            .unwrap();
        let storages = device.storages().await.unwrap();

        for (name, sent, want_unix) in cases {
            let info = crate::mtp::NewObjectInfo::file(name, 5).with_modified(sent);
            let handle = storages[0]
                .upload(None, info, bytes_stream(b"hello"))
                .await
                .unwrap();

            assert_eq!(mtime_unix(&dir.path().join(name)), want_unix, "{name}");
            let reported = storages[0].get_object_info(handle).await.unwrap();
            assert_eq!(
                reported
                    .modified
                    .unwrap()
                    .to_unix_seconds_with_fallback(offset(60)),
                Some(want_unix),
                "{name}"
            );
        }
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

    /// Android forgets a pending `SendObjectInfo` on any other command (AOSP
    /// `MtpServer::handleRequest`), so a listing from another task that lands between an
    /// upload's two phases makes its `SendObject` fail with `NoValidObjectInfo`, and on a real
    /// Pixel it wedged MTP until a replug (#62, `docs/specs/paced-uploads.md`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn upload_survives_listings_from_another_task() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        // The listers read a small folder, so each listing costs the same however many files
        // the uploads add to the root.
        std::fs::create_dir(dir.path().join("Listed")).unwrap();
        let device = MtpDevice::builder()
            .open_virtual(test_config(dir.path()))
            .await
            .unwrap();
        let uploader = device.storages().await.unwrap().remove(0);
        let listed = uploader
            .list_objects(None)
            .await
            .unwrap()
            .into_iter()
            .find(|o| o.filename == "Listed")
            .unwrap()
            .handle;

        // The gap is a few instructions wide (the futures mutex isn't fair, so the uploader
        // usually takes the lock straight back), and listers that have just started hit it most
        // often, so start a fresh one each round. Before the fix, 6 of 6 runs failed, with 2–6
        // of the 8,000 uploads refused; a run is ~7 s in a debug build.
        const ROUNDS: usize = 800;
        const UPLOADS_PER_ROUND: usize = 10;
        const LISTERS: usize = 1;
        let listings = Arc::new(AtomicUsize::new(0));
        let mut failures = Vec::new();
        for round in 0..ROUNDS {
            let done = Arc::new(AtomicBool::new(false));
            let mut tasks = Vec::new();
            for _ in 0..LISTERS {
                let lister = device.storages().await.unwrap().remove(0);
                let (done, listings) = (Arc::clone(&done), Arc::clone(&listings));
                tasks.push(tokio::spawn(async move {
                    while !done.load(Ordering::Relaxed) {
                        lister.list_objects(Some(listed)).await.unwrap();
                        listings.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            for i in 0..UPLOADS_PER_ROUND {
                let name = format!("r{round}-f{i}.txt");
                let info = crate::mtp::NewObjectInfo::file(name.clone(), 5);
                if let Err(e) = uploader.upload(None, info, bytes_stream(b"hello")).await {
                    failures.push((name, e.to_string()));
                }
            }
            done.store(true, Ordering::Relaxed);
            for task in tasks {
                task.await.unwrap();
            }
        }
        assert!(listings.load(Ordering::Relaxed) > 0);
        assert!(
            failures.is_empty(),
            "{} of {} uploads failed: {:?}",
            failures.len(),
            ROUNDS * UPLOADS_PER_ROUND,
            &failures[..failures.len().min(3)]
        );
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
    async fn upload_source_error_midway_leaves_nothing_on_android() {
        use crate::mtp::UploadError;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Download")).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder()
            .use_windowed_uploads(false)
            .open_virtual(config)
            .await
            .unwrap();
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

        let UploadError { source, partial } = err;
        assert!(
            matches!(source, crate::mtp::Error::Io { .. }),
            "expected the underlying I/O error, got {source:?}"
        );
        // Like AOSP, the device drops the object when the data phase ends short (#62), so
        // there's no partial for the caller to clean up, and the session answers right away.
        assert!(partial.is_none(), "the device already removed it");
        assert_upload_left_nothing(&storages[0], download.handle, "partial.bin", dir.path()).await;
    }

    /// After an upload that stopped partway: the next operation answers (on Android it used to
    /// time out, #62), and neither the object nor its file is left behind.
    async fn assert_upload_left_nothing(
        storage: &crate::mtp::Storage,
        folder: crate::mtp::ObjectHandle,
        name: &str,
        root: &std::path::Path,
    ) {
        let listed = storage
            .list_objects(Some(folder))
            .await
            .expect("the session must answer right after a stopped upload");
        assert!(listed.iter().all(|o| o.filename != name), "{listed:?}");
        assert!(!root.join("Download").join(name).exists());
    }

    #[tokio::test]
    async fn upload_dropped_midway_leaves_the_session_usable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Download")).unwrap();
        let device = MtpDevice::builder()
            .use_windowed_uploads(false)
            .open_virtual(test_config(dir.path()))
            .await
            .unwrap();
        let storages = device.storages().await.unwrap();
        let items = storages[0].list_objects(None).await.unwrap();
        let download = items.iter().find(|i| i.filename == "Download").unwrap();

        // A source that never finishes, dropped while the upload waits on it.
        let source = futures::StreamExt::chain(bytes_stream(b"abc"), futures::stream::pending());
        let info = crate::mtp::NewObjectInfo::file("dropped.bin", 100);
        let mut upload = Box::pin(storages[0].upload(Some(download.handle), info, source));
        assert!(futures::poll!(upload.as_mut()).is_pending());
        drop(upload);

        assert_upload_left_nothing(&storages[0], download.handle, "dropped.bin", dir.path()).await;
    }

    #[tokio::test]
    async fn upload_cancelled_midway_leaves_nothing_on_android() {
        use crate::mtp::UploadError;
        use std::ops::ControlFlow;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Download")).unwrap();

        let config = test_config(dir.path());
        let device = MtpDevice::builder()
            .use_windowed_uploads(false)
            .open_virtual(config)
            .await
            .unwrap();
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
        assert!(partial.is_none(), "the device already removed it");
        assert_upload_left_nothing(&storages[0], download.handle, "cancelled.bin", dir.path())
            .await;
    }

    /// Opens a camera-mode device with a `Download` folder; returns it, its storage, and the
    /// folder's handle.
    async fn open_camera_with_download_folder(
        dir: &std::path::Path,
    ) -> (MtpDevice, crate::mtp::Storage, crate::mtp::ObjectHandle) {
        std::fs::create_dir(dir.join("Download")).unwrap();
        let device = MtpDevice::builder()
            .open_virtual(camera_config(dir))
            .await
            .unwrap();
        let storage = device.storages().await.unwrap().remove(0);
        let items = storage.list_objects(None).await.unwrap();
        let folder = items.iter().find(|i| i.filename == "Download").unwrap();
        let handle = folder.handle;
        (device, storage, handle)
    }

    /// Off Android, CLASS_CANCEL ends the data phase and the device keeps the half-written
    /// object, so the caller gets its handle to delete or resume, and the library leaves it.
    async fn assert_partial_kept_and_deletable(
        storage: &crate::mtp::Storage,
        partial: Option<crate::mtp::ObjectHandle>,
        name: &str,
    ) {
        let handle = partial.expect("a camera keeps the object, so the handle must surface");
        let info = storage
            .get_object_info(handle)
            .await
            .expect("the half-written object stays at the surfaced handle");
        assert_eq!(info.filename, name);
        storage
            .delete(handle)
            .await
            .expect("the surfaced handle is deletable");
        assert!(storage.get_object_info(handle).await.is_err());
    }

    #[tokio::test]
    async fn upload_source_error_midway_surfaces_partial_on_a_camera() {
        let dir = tempfile::tempdir().unwrap();
        let (_device, storage, folder) = open_camera_with_download_folder(dir.path()).await;

        let info = crate::mtp::NewObjectInfo::file("partial.bin", 100);
        let err = storage
            .upload(Some(folder), info, failing_stream(b"abc"))
            .await
            .expect_err("upload should fail when the source stream errors mid-stream");
        assert!(matches!(err.source, crate::mtp::Error::Io { .. }));
        assert_partial_kept_and_deletable(&storage, err.partial, "partial.bin").await;
    }

    #[tokio::test]
    async fn upload_cancelled_midway_surfaces_partial_on_a_camera() {
        use std::ops::ControlFlow;

        let dir = tempfile::tempdir().unwrap();
        let (_device, storage, folder) = open_camera_with_download_folder(dir.path()).await;

        let info = crate::mtp::NewObjectInfo::file("cancelled.bin", 5);
        let err = storage
            .upload_with_progress(Some(folder), info, bytes_stream(b"hello"), |_| {
                ControlFlow::Break(())
            })
            .await
            .expect_err("cancelled upload should fail");
        assert!(matches!(err.source, crate::mtp::Error::Cancelled));
        assert_partial_kept_and_deletable(&storage, err.partial, "cancelled.bin").await;
    }

    #[tokio::test]
    async fn upload_dropped_midway_on_a_camera_keeps_the_object_and_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let (_device, storage, folder) = open_camera_with_download_folder(dir.path()).await;

        let source = futures::StreamExt::chain(bytes_stream(b"abc"), futures::stream::pending());
        let info = crate::mtp::NewObjectInfo::file("dropped.bin", 100);
        let mut upload = Box::pin(storage.upload(Some(folder), info, source));
        assert!(futures::poll!(upload.as_mut()).is_pending());
        drop(upload);

        // The next operation's recovery sends CLASS_CANCEL, which ends the data phase; the
        // object stays, as on a camera that keeps what it got.
        let listed = storage
            .list_objects(Some(folder))
            .await
            .expect("the session must answer right after a dropped upload");
        assert!(
            listed.iter().any(|o| o.filename == "dropped.bin"),
            "{listed:?}"
        );
        assert!(dir.path().join("Download/dropped.bin").exists());
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
        let (reported_old, new_handle) = crate::virtual_device::rekey_virtual_object(
            "rekey-test-001",
            std::path::Path::new("Download"),
        )
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
        assert!(crate::virtual_device::rekey_virtual_object(
            "no-such-serial",
            std::path::Path::new("Download")
        )
        .is_none());
        // Known device, untracked path -> None.
        assert!(crate::virtual_device::rekey_virtual_object(
            "rekey-test-none",
            std::path::Path::new("Nope")
        )
        .is_none());
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

    // -- Copy name clashes (#59) ---------------------------------------------------
    //
    // AOSP answers a `CopyObject` whose destination already holds that name with OK and handle
    // 0xFFFF, then writes the source over the existing file without truncating it. The virtual
    // device models that, so these tests pin that the façade refuses before the device sees it.

    #[tokio::test]
    async fn copy_into_its_own_folder_is_refused_and_leaves_the_original_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), "original bytes").unwrap();

        let config = test_config_with_serial(dir.path(), "copy-clash-same-folder");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let items = storages[0].list_objects(None).await.unwrap();
        let original = items.iter().find(|i| i.filename == "a.jpg").unwrap();

        let result = storages[0]
            .copy_object(original.handle, crate::mtp::ObjectHandle::ROOT, None)
            .await;
        assert!(
            matches!(result, Err(crate::mtp::Error::AlreadyExists)),
            "expected AlreadyExists, got {result:?}"
        );

        let after = storages[0].list_objects(None).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].handle, original.handle);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.jpg")).unwrap(),
            "original bytes"
        );
    }

    #[tokio::test]
    async fn copy_onto_a_same_named_file_elsewhere_is_refused_and_leaves_it_intact() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), "short").unwrap();
        std::fs::create_dir(dir.path().join("dest")).unwrap();
        std::fs::write(dir.path().join("dest/a.jpg"), "a much longer existing file").unwrap();

        let config = test_config_with_serial(dir.path(), "copy-clash-other-folder");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let items = storages[0].list_objects(None).await.unwrap();
        let source = items.iter().find(|i| i.filename == "a.jpg").unwrap();
        let dest = items.iter().find(|i| i.filename == "dest").unwrap();

        let result = storages[0]
            .copy_object(source.handle, dest.handle, None)
            .await;
        assert!(
            matches!(result, Err(crate::mtp::Error::AlreadyExists)),
            "expected AlreadyExists, got {result:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dest/a.jpg")).unwrap(),
            "a much longer existing file"
        );
    }

    #[tokio::test]
    async fn copy_onto_a_name_differing_only_in_case_is_refused() {
        // Android's shared storage is case-insensitive while `MtpStorageManager` matches names
        // exactly, so `A.JPG` next to `a.jpg` gets "copied" over the existing file.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), "source").unwrap();
        std::fs::create_dir(dir.path().join("dest")).unwrap();
        std::fs::write(dir.path().join("dest/A.JPG"), "existing").unwrap();

        let config = test_config_with_serial(dir.path(), "copy-clash-case");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let items = storages[0].list_objects(None).await.unwrap();
        let source = items.iter().find(|i| i.filename == "a.jpg").unwrap();
        let dest = items.iter().find(|i| i.filename == "dest").unwrap();

        let result = storages[0]
            .copy_object(source.handle, dest.handle, None)
            .await;
        assert!(
            matches!(result, Err(crate::mtp::Error::AlreadyExists)),
            "expected AlreadyExists, got {result:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dest/A.JPG")).unwrap(),
            "existing"
        );
    }

    #[tokio::test]
    async fn copy_returns_a_new_handle_in_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), "bytes").unwrap();
        std::fs::create_dir(dir.path().join("dest")).unwrap();

        let config = test_config_with_serial(dir.path(), "copy-new-handle");
        let device = MtpDevice::builder().open_virtual(config).await.unwrap();
        let storages = device.storages().await.unwrap();
        let items = storages[0].list_objects(None).await.unwrap();
        let source = items.iter().find(|i| i.filename == "a.jpg").unwrap();
        let dest = items.iter().find(|i| i.filename == "dest").unwrap();

        let copy = storages[0]
            .copy_object(source.handle, dest.handle, None)
            .await
            .unwrap();
        assert_ne!(copy, source.handle);
        let info = storages[0].get_object_info(copy).await.unwrap();
        assert_eq!(info.parent, dest.handle);
        assert_eq!(info.filename, "a.jpg");
    }

    /// The raw AOSP answer, one layer down: OK with handle 0xFFFF, no new object, and the
    /// destination overwritten in place without truncation.
    /// `SendObjectInfo`, any other command, then `SendObject`, on a raw session. Returns the
    /// `SendObject` result and whether the session answers afterwards.
    async fn send_object_after_another_command(
        android: bool,
        serial: &str,
    ) -> (Result<(), crate::PtpError>, bool, Option<bool>) {
        use crate::ptp::{ObjectFormatCode, ObjectHandle as PtpHandle, ObjectInfo, PtpSession};
        use crate::transport::virtual_device::VirtualTransport;
        use crate::transport::Transport;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config_with_serial(dir.path(), serial);
        config.android = android;
        let transport: Arc<dyn Transport> = Arc::new(VirtualTransport::new(config));
        let session = PtpSession::open(transport, 1).await.unwrap();
        let storage = session.get_storage_ids().await.unwrap()[0];
        let info = ObjectInfo {
            format: ObjectFormatCode::Undefined,
            size: 5,
            filename: "a.txt".into(),
            ..Default::default()
        };
        session
            .send_object_info(storage, PtpHandle::SEND_ROOT, &info)
            .await
            .unwrap();
        session.get_storage_ids().await.unwrap();
        let sent = session.send_object(b"hello").await;
        let healthy = session.get_storage_ids().await.is_ok();
        (
            sent,
            healthy,
            crate::virtual_device::virtual_device_wedged(serial),
        )
    }

    #[tokio::test]
    async fn send_object_after_another_command_is_refused_like_aosp() {
        let (sent, healthy, wedged) =
            send_object_after_another_command(true, "sendinfo-forgotten").await;
        assert!(
            matches!(
                sent,
                Err(crate::PtpError::Protocol {
                    code: crate::ptp::ResponseCode::Unknown(0x2015),
                    ..
                })
            ),
            "{sent:?}"
        );
        // The phone refused the data phase before reading it, so the data arrives as commands
        // and it stops answering: a Pixel needed a replug (#62).
        assert!(!healthy, "the refused data must wedge the device");
        assert_eq!(wedged, Some(true));
    }

    #[tokio::test]
    async fn send_object_after_another_command_still_works_on_a_camera() {
        let (sent, healthy, _) = send_object_after_another_command(false, "sendinfo-camera").await;
        assert!(sent.is_ok(), "{sent:?}");
        assert!(healthy);
    }

    #[tokio::test]
    async fn ptp_copy_onto_an_existing_name_answers_like_aosp() {
        use crate::ptp::{ObjectHandle as PtpHandle, PtpSession};
        use crate::transport::virtual_device::VirtualTransport;
        use crate::transport::Transport;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), "SHORT").unwrap();
        std::fs::create_dir(dir.path().join("dest")).unwrap();
        std::fs::write(dir.path().join("dest/a.jpg"), "existing long tail").unwrap();

        let config = test_config_with_serial(dir.path(), "copy-aosp-raw");
        let transport: Arc<dyn Transport> = Arc::new(VirtualTransport::new(config));
        let session = PtpSession::open(transport, 1).await.unwrap();
        let storage = session.get_storage_ids().await.unwrap()[0];
        let root = session
            .get_object_handles(storage, None, Some(PtpHandle::ROOT))
            .await
            .unwrap();
        let mut source = None;
        let mut dest = None;
        for handle in root {
            let info = session.get_object_info(handle).await.unwrap();
            match info.filename.as_str() {
                "a.jpg" => source = Some(handle),
                "dest" => dest = Some(handle),
                _ => {}
            }
        }
        let (source, dest) = (source.unwrap(), dest.unwrap());
        let before = session
            .get_object_handles(storage, None, Some(dest))
            .await
            .unwrap();

        let copy = session.copy_object(source, storage, dest).await.unwrap();
        assert_eq!(copy, PtpHandle(0x0000_FFFF));
        assert!(session.get_object_info(copy).await.is_err());

        let after = session
            .get_object_handles(storage, None, Some(dest))
            .await
            .unwrap();
        assert_eq!(before, after, "no new object appears");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dest/a.jpg")).unwrap(),
            "SHORTing long tail"
        );

        // Into its own folder: still 0xFFFF, and the file is untouched.
        let copy = session
            .copy_object(source, storage, PtpHandle::ROOT)
            .await
            .unwrap();
        assert_eq!(copy, PtpHandle(0x0000_FFFF));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.jpg")).unwrap(),
            "SHORT"
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

    // Every `fs_watcher_*` test below carries `#[serial_test::serial(fs_watcher)]`.
    //
    // They wait on real OS filesystem notifications, and each one spins up its own
    // `notify` watcher over its own tempdir. Run six of those concurrently inside a
    // 400-test parallel suite and macOS FSEvents delivery slips past the 5s budget
    // in `poll_event_with_retry`, so they fail as a group with "expected event from
    // fs watcher, got nothing" while passing every time in isolation. That reads
    // like a watcher bug and isn't one, which is the expensive part.
    //
    // The `fs_watcher` key scopes the serialization to these tests: they queue
    // behind each other, not behind every `#[serial]` test in the workspace. Don't
    // "fix" a future flake here by inflating the timeout; the contention is the
    // problem, and a longer timeout just makes the suite slower before it fails.

    /// Block until the filesystem watcher is actually armed, then leave the event
    /// queue empty.
    ///
    /// `notify` arms its FSEvents stream on a background thread, so a write issued
    /// straight after `open_virtual` can land *before* the watcher is listening and
    /// produce no event at all. The test then fails as "expected event from fs
    /// watcher, got nothing", which reads like a watcher bug rather than a race.
    ///
    /// So probe instead of guessing: write a throwaway file and wait for the
    /// watcher to report *something*. That's proof it's live, where a fixed sleep
    /// is only a hope. Then drain, so the caller's own assertion sees only its own
    /// event. Tests that already open with a drain loop get this for free, which is
    /// why they were the ones that didn't flake.
    async fn wait_for_watcher_ready(device: &MtpDevice, backing_dir: &std::path::Path) {
        let probe = backing_dir.join(".watcher-ready-probe");
        std::fs::write(&probe, b"probe").unwrap();
        assert!(
            poll_event_with_retry(device, Duration::from_secs(10))
                .await
                .is_some(),
            "fs watcher never armed"
        );
        // The probe file STAYS. Deleting it queues an `ObjectRemoved` that FSEvents
        // can deliver after the drain below has already declared things quiet, and
        // the caller's first polled event is then the probe's removal rather than
        // its own creation ("expected ObjectAdded, got ObjectRemoved"). Leaving one
        // stray file in a tempdir costs nothing; the late removal costs an hour.
        while poll_event_with_retry(device, Duration::from_millis(500))
            .await
            .is_some()
        {}
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
    #[serial_test::serial(fs_watcher)]
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
        wait_for_watcher_ready(&device, &backing_dir).await;

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

        // Prove the watcher is live before handing the device back. The bare drain
        // this used to do only *hoped* it was, which is the same arming race that
        // made `fs_watcher_detects_file_creation` flake.
        wait_for_watcher_ready(&device, &backing_dir).await;

        (device, backing_dir, dir)
    }

    #[tokio::test]
    #[serial_test::serial(fs_watcher)]
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
    #[serial_test::serial(fs_watcher)]
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
    #[serial_test::serial(fs_watcher)]
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
    #[serial_test::serial(fs_watcher)]
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
        wait_for_watcher_ready(&device, &backing_dir).await;

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
    #[serial_test::serial(fs_watcher)]
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
        // Not just anti-flake here: this test asserts the watcher does NOT emit a
        // duplicate, so an unarmed watcher would pass it for entirely the wrong
        // reason. Proving the watcher is live is what makes the assertion mean
        // something.
        wait_for_watcher_ready(&device, &backing_dir).await;
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
        let summary = crate::virtual_device::rescan_virtual_device("rescan-test-001").unwrap();
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

        let summary = crate::virtual_device::rescan_virtual_device("rescan-test-002").unwrap();
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

    // -- GetObjectPropList listings vs per-handle listings -------------------------

    /// Nested folders, empty folders, a few formats, and fixed dates.
    fn listing_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for folder in ["DCIM/Camera", "DCIM/Empty", "Music", "Docs"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        let files = [
            ("top.txt", &b"top"[..]),
            ("DCIM/Camera/IMG_0001.jpg", b"jpeg one"),
            ("DCIM/Camera/IMG_0002.jpg", b"jpeg number two"),
            ("DCIM/Camera/clip.mp4", b"not really a video"),
            ("Music/song.mp3", b"la la la"),
        ];
        for (i, (rel, bytes)) in files.into_iter().enumerate() {
            let path = root.join(rel);
            std::fs::write(&path, bytes).unwrap();
            set_mtime(&path, 1_710_513_022 + 3_600 * i as i64);
        }
        dir
    }

    async fn open_listing_device(
        dir: &std::path::Path,
        serial: &str,
        use_prop_list: bool,
        tweak: impl FnOnce(&mut VirtualDeviceConfig),
    ) -> MtpDevice {
        let mut config = VirtualDeviceConfig {
            utc_offset: offset(120),
            ..test_config_with_serial(dir, serial)
        };
        tweak(&mut config);
        MtpDevice::builder()
            .use_object_prop_list(use_prop_list)
            .open_virtual(config)
            .await
            .unwrap()
    }

    /// Every object on the first storage, by path, listing each folder on its own.
    async fn walk(device: &MtpDevice) -> Vec<(String, crate::mtp::ObjectInfo)> {
        let storage = device.storages().await.unwrap().remove(0);
        let mut out = Vec::new();
        let mut folders = vec![(None, String::new())];
        while let Some((parent, prefix)) = folders.pop() {
            for object in storage.list_objects(parent).await.unwrap() {
                let path = format!("{prefix}/{}", object.filename);
                if object.is_folder() {
                    folders.push((Some(object.handle), path.clone()));
                }
                out.push((path, object));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Assert two walks list the same objects with the same fields.
    fn assert_same_listing(
        expected: &[(String, crate::mtp::ObjectInfo)],
        got: &[(String, crate::mtp::ObjectInfo)],
        label: &str,
    ) {
        let paths = |walk: &[(String, crate::mtp::ObjectInfo)]| {
            walk.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>()
        };
        assert_eq!(paths(expected), paths(got), "{label}: different objects");
        for ((path, e), (_, g)) in expected.iter().zip(got) {
            assert_eq!(e.handle, g.handle, "{label}: {path} handle");
            assert_eq!(e.storage_id, g.storage_id, "{label}: {path} storage");
            assert_eq!(e.parent, g.parent, "{label}: {path} parent");
            assert_eq!(e.filename, g.filename, "{label}: {path} name");
            assert_eq!(e.size, g.size, "{label}: {path} size");
            assert_eq!(e.format, g.format, "{label}: {path} format");
            assert_eq!(e.is_folder(), g.is_folder(), "{label}: {path} folder");
            assert_eq!(e.modified, g.modified, "{label}: {path} modified");
            assert_eq!(e.created, g.created, "{label}: {path} created");
            assert_eq!(e.image_width, g.image_width, "{label}: {path} width");
            assert_eq!(e.image_height, g.image_height, "{label}: {path} height");
        }
    }

    fn prop_list_requests(serial: &str) -> u32 {
        super::registry::operation_count(serial, OperationCode::GetObjectPropList).unwrap()
    }

    fn object_info_requests(serial: &str) -> u32 {
        super::registry::operation_count(serial, OperationCode::GetObjectInfo).unwrap()
    }

    #[tokio::test]
    async fn a_folder_listing_uses_get_object_prop_list_unless_turned_off() {
        let dir = listing_tree();
        let camera_files = 3;

        for (use_prop_list, serial) in [(true, "prop-list-on"), (false, "prop-list-off")] {
            let device = open_listing_device(dir.path(), serial, use_prop_list, |_| {}).await;
            let storage = device.storages().await.unwrap().remove(0);
            let dcim = storage.list_objects(None).await.unwrap();
            let dcim = dcim.iter().find(|o| o.filename == "DCIM").unwrap().handle;
            let camera = storage.list_objects(Some(dcim)).await.unwrap();
            let camera = camera
                .iter()
                .find(|o| o.filename == "Camera")
                .unwrap()
                .handle;

            let (prop_lists, infos) = (prop_list_requests(serial), object_info_requests(serial));
            let listed = storage.list_objects(Some(camera)).await.unwrap();
            assert_eq!(listed.len(), camera_files);
            let prop_lists = prop_list_requests(serial) - prop_lists;
            let infos = object_info_requests(serial) - infos;
            if use_prop_list {
                // One request per property: the required four, then the two dates.
                assert_eq!((prop_lists, infos), (6, 0), "no GetObjectInfo");
            } else {
                assert_eq!((prop_lists, infos), (0, 3), "one GetObjectInfo per child");
            }
        }
    }

    #[tokio::test]
    async fn both_listing_paths_agree_field_by_field() {
        let dir = listing_tree();
        let off = open_listing_device(dir.path(), "agree-off", false, |_| {}).await;
        let per_handle = walk(&off).await;
        assert_eq!(per_handle.len(), 10, "the whole tree");
        assert!(per_handle
            .iter()
            .all(|(_, o)| o.modified.is_some() && o.created.is_some()));

        let device = open_listing_device(dir.path(), "agree-on", true, |_| {}).await;
        assert_same_listing(&per_handle, &walk(&device).await, "per property");
        assert!(prop_list_requests("agree-on") > 0);

        // The device itself not advertising the operation is the same as turning it off.
        let device = open_listing_device(dir.path(), "agree-unadvertised", true, |c| {
            c.supports_object_prop_list = false;
        })
        .await;
        assert_same_listing(&per_handle, &walk(&device).await, "unadvertised");
        assert_eq!(prop_list_requests("agree-unadvertised"), 0);
    }

    #[tokio::test]
    async fn a_declined_required_property_lists_per_handle() {
        let dir = listing_tree();
        let per_handle =
            walk(&open_listing_device(dir.path(), "decl-req-off", false, |_| {}).await).await;

        let serial = "decl-req-on";
        let device = open_listing_device(dir.path(), serial, true, |c| {
            c.prop_list_declined_properties = vec![crate::ptp::ObjectPropertyCode::ObjectFormat];
        })
        .await;
        assert_same_listing(&per_handle, &walk(&device).await, "required declined");

        // Once the device has shown it can't serve a listing, it isn't asked again.
        let storage = device.storages().await.unwrap().remove(0);
        let music = storage.list_objects(None).await.unwrap();
        let music = music.iter().find(|o| o.filename == "Music").unwrap().handle;
        let before = prop_list_requests(serial);
        storage.list_objects(Some(music)).await.unwrap();
        assert_eq!(prop_list_requests(serial), before);
    }

    #[tokio::test]
    async fn a_stale_folder_handle_does_not_turn_the_fast_path_off() {
        let dir = listing_tree();
        let serial = "prop-list-rekey";
        let device = open_listing_device(dir.path(), serial, true, |_| {}).await;
        let storage = device.storages().await.unwrap().remove(0);
        let find = |objects: Vec<crate::mtp::ObjectInfo>, name: &str| {
            objects
                .into_iter()
                .find(|o| o.filename == name)
                .unwrap()
                .handle
        };
        let dcim = find(storage.list_objects(None).await.unwrap(), "DCIM");
        let stale = find(storage.list_objects(Some(dcim)).await.unwrap(), "Camera");

        // Android re-keys handles after a media rescan: the cached handle is now unknown.
        super::registry::rekey_virtual_object(serial, std::path::Path::new("DCIM/Camera")).unwrap();
        // A stale handle must fail, never read as an empty folder.
        let listed = storage.list_objects(Some(stale)).await;
        assert!(
            matches!(listed, Err(Error::StaleHandle)),
            "expected StaleHandle, got {listed:?}"
        );

        // Re-listing the parent finds the new handle, and that listing still takes the
        // prop-list path: six per-property requests, no GetObjectInfo.
        let fresh = find(storage.list_objects(Some(dcim)).await.unwrap(), "Camera");
        assert_ne!(fresh, stale);
        let (prop_lists, infos) = (prop_list_requests(serial), object_info_requests(serial));
        assert_eq!(storage.list_objects(Some(fresh)).await.unwrap().len(), 3);
        assert_eq!(
            (
                prop_list_requests(serial) - prop_lists,
                object_info_requests(serial) - infos
            ),
            (6, 0)
        );
    }

    #[tokio::test]
    async fn an_undescribable_object_in_a_folder_is_skipped_on_the_prop_list_path_too() {
        let dir = listing_tree();
        let serial = "prop-list-22";
        let device = open_listing_device(dir.path(), serial, true, |c| {
            c.undescribable_objects = vec!["DCIM/Camera/IMG_0002.jpg".into()];
        })
        .await;
        let storage = device.storages().await.unwrap().remove(0);
        let dcim = storage.list_objects(None).await.unwrap();
        let dcim = dcim.iter().find(|o| o.filename == "DCIM").unwrap().handle;
        let camera = storage.list_objects(Some(dcim)).await.unwrap();
        let camera = camera
            .iter()
            .find(|o| o.filename == "Camera")
            .unwrap()
            .handle;

        let before = prop_list_requests(serial);
        let collection = storage.collect_objects(Some(camera)).await.unwrap();
        assert!(
            prop_list_requests(serial) > before,
            "the prop-list path was tried first"
        );
        let mut names: Vec<_> = collection
            .objects
            .iter()
            .map(|o| o.filename.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["IMG_0001.jpg", "clip.mp4"]);
        assert_eq!(collection.skipped.len(), 1);

        // One bad folder doesn't turn the fast path off for the others.
        let music = storage.list_objects(None).await.unwrap();
        let music = music.iter().find(|o| o.filename == "Music").unwrap().handle;
        let (prop_lists, infos) = (prop_list_requests(serial), object_info_requests(serial));
        assert_eq!(storage.list_objects(Some(music)).await.unwrap().len(), 1);
        assert_eq!(
            (
                prop_list_requests(serial) - prop_lists,
                object_info_requests(serial) - infos
            ),
            (6, 0),
            "one request per property, no GetObjectInfo"
        );
    }
}
