//! # mtp-rs
//!
//! A pure-Rust MTP (Media Transfer Protocol) library targeting modern Android devices.
//!
//! ## Features
//!
//! - **Runtime agnostic**: Uses `futures` traits, works with any async runtime
//! - **Two-level API**: High-level `mtp::` for media devices, low-level `ptp::` for cameras
//! - **Streaming**: Memory-efficient streaming downloads with progress tracking
//! - **Type safe**: Newtype wrappers prevent mixing up IDs
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use mtp_rs::mtp::MtpDevice;
//!
//! # async fn example() -> Result<(), mtp_rs::Error> {
//! // Open the first MTP device
//! let device = MtpDevice::open_first().await?;
//!
//! println!("Connected to: {} {}",
//!          device.device_info().manufacturer,
//!          device.device_info().model);
//!
//! // Get storages
//! for storage in device.storages().await? {
//!     println!("Storage: {} ({} free)",
//!              storage.info().description,
//!              storage.info().free_space);
//!
//!     // List root folder
//!     for obj in storage.list_objects(None).await? {
//!         let kind = if obj.is_folder() { "DIR " } else { "FILE" };
//!         println!("  {} {} ({} bytes)", kind, obj.filename, obj.size);
//!     }
//! }
//! # Ok(())
//! # }
//! ```

#[macro_use]
mod trace;

pub mod cancel;
pub mod error;
pub mod mtp;
pub mod ptp;
pub mod transport;

pub use cancel::CancelToken;

// The crate root surfaces the backend-neutral high-level API. `Error` is the neutral
// `mtp::Error`; the rich low-level PTP error stays available as `PtpError` (and under `ptp::`).
pub use error::PtpError;
pub use mtp::{Error, UploadError};

// Backend-neutral high-level types (the default vocabulary for `mtp::`).
pub use mtp::{
    Backend, ByteRange, Capabilities, DateTime, DeviceEvent, DeviceInfo, FileDownload, ListingItem,
    MtpDevice, MtpDeviceBuilder, NewObjectInfo, ObjectCollection, ObjectFormat, ObjectHandle,
    ObjectInfo, ObjectListing, Progress, SkippedObject, Storage, StorageId, StorageInfo,
    WindowedDownload, DEFAULT_CANCEL_TIMEOUT, DEFAULT_DOWNLOAD_WINDOW,
};

// Low-level PTP escape hatch for cameras / protocol work. These keep their PTP-specific
// shapes; reach for them via `ptp::` when the neutral surface isn't enough.
pub use ptp::{receive_stream_to_stream, EventCode, OperationCode, ReceiveStream, ResponseCode};

// Re-export USB speed enum for callers that want to surface link speed (e.g. for UI).
pub use transport::UsbSpeed;

// Re-export virtual device types when the feature is enabled
#[cfg(feature = "virtual-device")]
pub use transport::virtual_device::config::{VirtualDeviceConfig, VirtualStorageConfig};
#[cfg(feature = "virtual-device")]
pub use transport::virtual_device::registry::{
    clear_dropped_paths, dropped_paths_since_pause, force_cancel_wedge, force_operation_wedge,
    force_partial_read_caps, pause_watcher, register_virtual_device, rekey_virtual_object,
    rescan_virtual_device, unregister_virtual_device, was_path_dropped, WatcherGuard,
};
#[cfg(feature = "virtual-device")]
pub use transport::virtual_device::RescanSummary;
#[cfg(feature = "virtual-device")]
pub use transport::virtual_device::DROPPED_PATHS_CAP;
