//! A filesystem-backed virtual MTP device, for testing without hardware. **Exempt from SemVer.**
//!
//! Needs the `virtual-device` feature. This is test support: the config fields and the hooks here
//! (fault injection, watcher control, stale-handle simulation) can change in any release, so treat
//! them like your test fixtures and expect to adjust when you bump `mtp-rs`.
//!
//! Open a device directly with [`MtpDeviceBuilder::open_virtual`](crate::MtpDeviceBuilder::open_virtual),
//! or call [`register_virtual_device`] so that [`MtpDevice::list_devices`](crate::MtpDevice::list_devices)
//! and the `open_by_*` methods find it like a plugged-in device.
//!
//! Build a config by stating the fields you care about and spreading the rest:
//!
//! ```rust,no_run
//! use mtp_rs::virtual_device::{VirtualDeviceConfig, VirtualStorageConfig};
//! use std::path::PathBuf;
//!
//! let config = VirtualDeviceConfig {
//!     storages: vec![VirtualStorageConfig {
//!         description: "Internal Storage".into(),
//!         capacity: 1024 * 1024,
//!         backing_dir: PathBuf::from("/tmp/mtp-test"),
//!         read_only: false,
//!     }],
//!     ..Default::default()
//! };
//! ```

pub use crate::transport::virtual_device::config::{VirtualDeviceConfig, VirtualStorageConfig};
pub use crate::transport::virtual_device::registry::{
    clear_dropped_paths, clear_object_info_errors, dropped_paths_since_pause, force_cancel_wedge,
    force_object_info_error, force_operation_wedge, force_partial_read_caps, pause_watcher,
    register_virtual_device, rekey_virtual_object, rescan_virtual_device,
    unregister_virtual_device, was_path_dropped, WatcherGuard,
};
pub use crate::transport::virtual_device::{RescanSummary, DROPPED_PATHS_CAP};
