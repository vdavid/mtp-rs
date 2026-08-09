//! Configuration types for virtual MTP devices.

use std::path::PathBuf;
use std::time::Duration;

/// Configuration for a virtual MTP device.
///
/// Defines the identity and storages of a virtual device that operates against
/// a local filesystem directory instead of real USB hardware.
///
/// Build one from [`Default`] and set only the fields your test cares about.
/// New fields then arrive with a working default instead of breaking every
/// construction site.
///
/// # Example
///
/// ```rust
/// use std::path::PathBuf;
/// use mtp_rs::transport::virtual_device::config::{VirtualDeviceConfig, VirtualStorageConfig};
///
/// let config = VirtualDeviceConfig {
///     manufacturer: "Google".into(),
///     model: "Virtual Pixel 9".into(),
///     serial: "virtual-001".into(),
///     storages: vec![VirtualStorageConfig {
///         description: "Internal Storage".into(),
///         capacity: 64 * 1024 * 1024 * 1024,
///         backing_dir: PathBuf::from("/tmp/mtp-test"),
///         read_only: false,
///     }],
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct VirtualDeviceConfig {
    /// Manufacturer name reported by the virtual device.
    pub manufacturer: String,
    /// Model name reported by the virtual device.
    pub model: String,
    /// Serial number for the virtual device.
    pub serial: String,
    /// Storage configurations. At least one storage is required.
    pub storages: Vec<VirtualStorageConfig>,
    /// Whether the device advertises SetObjectPropValue support (rename).
    pub supports_rename: bool,
    /// Whether the device advertises `GetPartialObject64` (0x95C1), the 64-bit
    /// offset partial read. Real Android devices do; many PTP cameras (e.g. the
    /// Panasonic Lumix DMC-TZ61) only advertise the 32-bit `GetPartialObject`.
    /// Set `false` to model such a camera and exercise the 32-bit fallback path.
    /// The 32-bit `GetPartialObject` is always advertised regardless.
    pub supports_partial_object_64: bool,
    /// How long `receive_interrupt` waits when no events are pending.
    /// Simulates the USB interrupt endpoint blocking behavior.
    /// Default: 50ms for production use. Use `Duration::ZERO` in tests
    /// to avoid slowing down the test suite.
    pub event_poll_interval: Duration,
    /// Watch backing directories for out-of-band changes and emit MTP events.
    /// When `true`, a background filesystem watcher detects files created or
    /// removed directly in the backing directories (bypassing MTP) and queues
    /// `ObjectAdded`/`ObjectRemoved` events. Set to `false` for tests that
    /// don't need this (faster startup, no background threads).
    pub watch_backing_dirs: bool,
    /// Storage-relative paths (`"b.txt"`, `"sub/c.txt"`) whose `GetObjectInfo`
    /// answers `GeneralError` instead of an ObjectInfo dataset. The objects stay
    /// present on disk and readable by every other operation, modeling a device
    /// that enumerates a handle and then won't describe it (Sphaira on the
    /// Nintendo Switch, issue #22).
    ///
    /// This is the config twin of
    /// [`force_object_info_error`](super::registry::force_object_info_error): the
    /// hook needs a handle, so it can only be armed after a listing and from
    /// inside the same process. Name the paths here instead when the device has
    /// to come up already broken, which is what a consumer testing its own
    /// binary end to end (a CLI, a FUSE mount) needs.
    ///
    /// Defaults to empty: every object describes itself.
    pub undescribable_objects: Vec<String>,
}

/// A ready-to-extend starting point: an obviously-fake identity plus the
/// behavior flags a modern Android device would report.
///
/// **You must set `storages`**: the default is empty, which is not a usable
/// device. `MtpDeviceBuilder::open_virtual` rejects it up front with
/// "VirtualDeviceConfig requires at least one storage", and
/// `register_virtual_device` produces a device with nothing on it. There is no
/// honest default here, since only the caller knows which directory backs the
/// storage.
///
/// Two other fields are worth overriding:
///
/// - `serial` defaults to a fixed string, so give each device its own when you
///   register more than one at a time.
/// - `event_poll_interval` and `watch_backing_dirs` default to production-like
///   values (50 ms, watching on). Tests that don't exercise the watcher run
///   faster with `Duration::ZERO` and `false`.
impl Default for VirtualDeviceConfig {
    fn default() -> Self {
        Self {
            manufacturer: "mtp-rs".into(),
            model: "Virtual Device".into(),
            serial: "virtual-0001".into(),
            storages: Vec::new(),
            supports_rename: true,
            supports_partial_object_64: true,
            event_poll_interval: Duration::from_millis(50),
            watch_backing_dirs: true,
            undescribable_objects: Vec::new(),
        }
    }
}

/// Configuration for a single storage within a virtual device.
///
/// Deliberately has no `Default`, unlike [`VirtualDeviceConfig`]: an unset
/// `backing_dir` is never caught. Nothing validates it, so the storage just
/// reports zero objects and zero used space, and the test fails somewhere far
/// from the cause. A compile error is the better outcome here.
#[derive(Debug, Clone)]
pub struct VirtualStorageConfig {
    /// Human-readable storage description (for example, "Internal Storage").
    pub description: String,
    /// Maximum storage capacity in bytes.
    pub capacity: u64,
    /// Local directory backing this storage. Files here become MTP objects.
    pub backing_dir: PathBuf,
    /// If true, write operations return `StoreReadOnly`.
    pub read_only: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_complete_except_storages() {
        let config = VirtualDeviceConfig::default();

        // A device listing must never show blanks.
        assert!(!config.manufacturer.is_empty());
        assert!(!config.model.is_empty());
        assert!(!config.serial.is_empty());

        // The one field the caller must fill in: only they know the backing dir.
        // `MtpDeviceBuilder::open_virtual` rejects an empty `storages`.
        assert!(config.storages.is_empty());

        assert!(config.supports_rename);
        assert!(config.supports_partial_object_64);
        assert_eq!(config.event_poll_interval, Duration::from_millis(50));
        assert!(config.watch_backing_dirs);
    }
}
