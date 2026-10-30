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
//!
//! ## Stability
//!
//! SemVer covers what the crate root re-exports, plus [`mtp`] and [`ptp`]. These optional features
//! are exempt from it, so they can change in any release:
//!
//! - `unstable-raw`: `transport::` and the other `nusb`-typed items, for when you enumerate with
//!   `nusb` yourself or bring your own transport. Listing devices and opening by serial or location
//!   need no raw types.
//! - `virtual-device`: `virtual_device::`, a filesystem-backed device for testing without hardware.
//! - `tracing`: structured diagnostics. The event names and fields can change, as can log output.

#[macro_use]
mod trace;

mod cancel;
mod error;
pub mod mtp;
pub mod ptp;
#[cfg(feature = "virtual-device")]
pub mod virtual_device;

#[cfg(feature = "unstable-raw")]
pub mod transport;
#[cfg(not(feature = "unstable-raw"))]
mod transport;

pub use cancel::CancelToken;

// The crate root surfaces the backend-neutral high-level API. `Error` is the neutral `mtp::Error`;
// the rich low-level PTP error lives at `ptp::PtpError`.
pub(crate) use error::PtpError;
pub use mtp::{Error, UploadError};

// Backend-neutral high-level types (the default vocabulary for `mtp::`). These root paths are the
// SemVer promise; the nested `mtp::` paths work too but aren't what the promise is about.
pub use mtp::{
    watch_devices, Backend, ByteRange, Capabilities, DateTime, DeviceEvent, DeviceInfo,
    DeviceWatch, DeviceWatchBuilder, FileDownload, FilesystemType, HotplugEvent, ListingItem,
    MtpDevice, MtpDeviceBuilder, MtpDeviceInfo, NewObjectInfo, ObjectCollection, ObjectFormat,
    ObjectHandle, ObjectInfo, ObjectListing, Progress, SkippedObject, Storage, StorageId,
    StorageInfo, StorageType, UtcOffset, WindowedDownload, DEFAULT_CANCEL_TIMEOUT,
    DEFAULT_DOWNLOAD_WINDOW, DEFAULT_SETTLE_DELAY, MAX_NAME_UNITS,
};

// The two USB-flavored facts `MtpDeviceInfo` carries: link speed (for example to warn about a slow
// cable in a UI) and why the device counted as MTP. Neither exposes `nusb`.
pub use transport::{MtpMatchReason, UsbSpeed};
