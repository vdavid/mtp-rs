//! Global registry of virtual devices for discovery integration.
//!
//! Virtual devices registered here appear in `MtpDevice::list_devices()` and
//! can be opened via `open_by_location()` or `open_by_serial()`.

use super::config::VirtualDeviceConfig;
use super::handlers::GENERAL_ERROR;
use super::state::{RescanSummary, VirtualDeviceState};
use crate::mtp::MtpDeviceInfo;
use crate::ptp::ObjectHandle;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Base for synthetic location IDs (high range, won't collide with real USB).
const VIRTUAL_LOCATION_BASE: u64 = 0xFFFF_0000_0000_0000;

/// A registered virtual device.
struct VirtualRegistration {
    info: MtpDeviceInfo,
    config: VirtualDeviceConfig,
}

/// Holds registered devices and a monotonically increasing index for unique location IDs.
struct Registry {
    devices: Vec<VirtualRegistration>,
    next_index: u64,
}

/// Access the global registry singleton.
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            devices: Vec::new(),
            next_index: 0,
        })
    })
}

/// Register a virtual device so it appears in `MtpDevice::list_devices()`.
///
/// Returns the `MtpDeviceInfo` with a synthetic location ID. Use this location
/// ID with `MtpDevice::open_by_location()` or the serial with
/// `MtpDevice::open_by_serial()` to open the device.
#[must_use]
pub fn register_virtual_device(config: &VirtualDeviceConfig) -> MtpDeviceInfo {
    let mut reg = registry().lock().unwrap();
    let index = reg.next_index;
    reg.next_index += 1;
    let location_id = VIRTUAL_LOCATION_BASE + index;

    let info = MtpDeviceInfo {
        vendor_id: 0xFFFF,
        product_id: 0x0001,
        manufacturer: Some(config.manufacturer.clone()),
        product: Some(config.model.clone()),
        serial_number: Some(config.serial.clone()),
        location_id,
        speed: None,
        match_reason: crate::transport::MtpMatchReason::KnownVidPid,
    };

    reg.devices.push(VirtualRegistration {
        info: info.clone(),
        config: config.clone(),
    });

    info
}

/// Remove a registered virtual device by location ID.
pub fn unregister_virtual_device(location_id: u64) {
    let mut reg = registry().lock().unwrap();
    reg.devices.retain(|r| r.info.location_id != location_id);
}

/// Get all registered virtual devices (called by `list_devices`).
pub(crate) fn list_virtual_devices() -> Vec<MtpDeviceInfo> {
    let reg = registry().lock().unwrap();
    reg.devices.iter().map(|r| r.info.clone()).collect()
}

/// Try to find a virtual device config by location ID.
pub(crate) fn find_virtual_config_by_location(location_id: u64) -> Option<VirtualDeviceConfig> {
    let reg = registry().lock().unwrap();
    reg.devices
        .iter()
        .find(|r| r.info.location_id == location_id)
        .map(|r| r.config.clone())
}

/// Try to find a virtual device config by serial number.
pub(crate) fn find_virtual_config_by_serial(serial: &str) -> Option<VirtualDeviceConfig> {
    let reg = registry().lock().unwrap();
    reg.devices
        .iter()
        .find(|r| r.info.serial_number.as_deref() == Some(serial))
        .map(|r| r.config.clone())
}

// --- Active device state registry ---
//
// When a VirtualTransport is created, it registers its shared state here so
// that `rescan_virtual_device()` can look it up by serial number. Entries are
// removed when the transport is dropped.

/// An entry in the active-states registry: (serial, shared state).
type ActiveEntry = (String, Arc<Mutex<VirtualDeviceState>>);

/// Access the global active-states registry.
fn active_states() -> &'static Mutex<Vec<ActiveEntry>> {
    static ACTIVE: OnceLock<Mutex<Vec<ActiveEntry>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register an active virtual device's state (called by `VirtualTransport::new`).
pub(super) fn register_active_state(serial: String, state: Arc<Mutex<VirtualDeviceState>>) {
    let mut active = active_states().lock().unwrap();
    active.push((serial, state));
}

/// Unregister an active virtual device's state (called when `VirtualTransport` is dropped).
pub(super) fn unregister_active_state(serial: &str) {
    let mut active = active_states().lock().unwrap();
    if let Some(pos) = active.iter().position(|(s, _)| s == serial) {
        active.remove(pos);
    }
}

/// RAII guard that decrements the filesystem watcher's pause refcount when
/// dropped. Resume happens only when the refcount hits zero, so multiple
/// concurrent test drains compose correctly: each `pause_watcher` call
/// increments, each guard drop decrements, and events only flow again once
/// every guard is gone.
///
/// Created by [`pause_watcher`]. While at least one guard is alive, all
/// filesystem events for the device are recorded in `dropped_paths` (capped)
/// and silently dropped from the live event queue.
pub struct WatcherGuard {
    state: Arc<Mutex<VirtualDeviceState>>,
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.pause_count = state.pause_count.saturating_sub(1);
        }
    }
}

/// Pause the filesystem watcher for a virtual device, returning a guard that
/// decrements the pause refcount on drop. The watcher actually resumes only
/// when the last guard drops.
///
/// While paused, filesystem events are silently dropped from the live event
/// queue AND recorded in the device's `dropped_paths` ring buffer so callers
/// can observe ordering via [`was_path_dropped`]. This prevents stale OS-level
/// events from corrupting the object tree when files in the backing directory
/// are deleted and recreated externally (outside of MTP), and lets test code
/// know when those events have actually drained (rather than guessing with a
/// fixed sleep).
///
/// Returns `None` if no active virtual device with that serial exists.
///
/// # Example
///
/// ```rust,no_run
/// use mtp_rs::{pause_watcher, rescan_virtual_device};
///
/// {
///     let _guard = pause_watcher("my-device-serial").unwrap();
///
///     // ... delete and recreate files in the backing directory ...
///
///     rescan_virtual_device("my-device-serial");
/// } // watcher refcount decremented here when `_guard` is dropped
/// ```
pub fn pause_watcher(serial: &str) -> Option<WatcherGuard> {
    let active = active_states().lock().unwrap();
    let state_arc = active
        .iter()
        .find(|(s, _)| s == serial)
        .map(|(_, state)| Arc::clone(state))?;
    drop(active);
    state_arc.lock().unwrap().pause_count += 1;
    Some(WatcherGuard { state: state_arc })
}

/// Returns the canonical paths the watcher has dropped (and recorded) while
/// paused, oldest first. The ring is capped at
/// [`DROPPED_PATHS_CAP`](super::state::DROPPED_PATHS_CAP) entries; once the
/// cap is reached, the oldest entry is evicted on every new push. Cheap
/// clone-out of the ring.
///
/// **This is the primary observation primitive** for test harnesses that
/// want event-driven confirmation that a backing-dir mutation has been
/// observed by the watcher. Compose patterns on top of it:
///
/// - **Sentinel-file drain**: write a uniquely-named file as the LAST
///   fixture step, poll until a returned path ends with that name.
///   Per-directory FS-event ordering on every supported `notify` backend
///   means every earlier write to the same directory already arrived.
///   See [`was_path_dropped`] for a thin convenience wrapper.
/// - **Event-count quiescence**: snapshot `.len()`, wait, snapshot again;
///   declare quiet when the count hasn't grown for N polls.
/// - **Per-subdir filter**: count only events under a specific directory.
///
/// Returns an empty `Vec` if no active virtual device with that serial
/// exists, so callers can poll without `Option` plumbing.
pub fn dropped_paths_since_pause(serial: &str) -> Vec<PathBuf> {
    let active = active_states().lock().unwrap();
    let state_arc = match active
        .iter()
        .find(|(s, _)| s == serial)
        .map(|(_, s)| Arc::clone(s))
    {
        Some(s) => s,
        None => return Vec::new(),
    };
    drop(active);
    let state = state_arc.lock().unwrap();
    state.dropped_paths.iter().cloned().collect()
}

/// Returns `true` if any path the watcher dropped while paused ends with
/// `suffix`. Thin convenience over [`dropped_paths_since_pause`] for the
/// common sentinel-file drain pattern: write a uniquely-named file as the
/// LAST fixture step, then poll this until it returns `true`.
///
/// The suffix match avoids the caller having to canonicalize the path
/// (macOS `/tmp` ↔ `/private/tmp`, the watcher's own canonicalization).
/// Pass a sufficiently unique suffix (a UUID-bearing filename) so concurrent
/// drains don't false-positive on each other's sentinels.
///
/// Returns `false` if no active virtual device with that serial exists.
pub fn was_path_dropped(serial: &str, suffix: &str) -> bool {
    dropped_paths_since_pause(serial)
        .iter()
        .any(|p| p.to_string_lossy().ends_with(suffix))
}

/// Clears the recorded `dropped_paths` ring buffer for the device. Cheap;
/// call it after a successful drain + rescan so the ring stays scoped to
/// in-flight pauses rather than accumulating across long-running test
/// sessions.
pub fn clear_dropped_paths(serial: &str) {
    let active = active_states().lock().unwrap();
    let state_arc = match active
        .iter()
        .find(|(s, _)| s == serial)
        .map(|(_, s)| Arc::clone(s))
    {
        Some(s) => s,
        None => return,
    };
    drop(active);
    state_arc.lock().unwrap().dropped_paths.clear();
}

/// Force a rescan of a virtual device's backing directories, identified by
/// serial number.
///
/// This diffs the in-memory object tree against the actual filesystem and
/// queues `ObjectAdded`/`ObjectRemoved`/`StorageInfoChanged` events for any
/// differences found.
///
/// Returns `Some(summary)` with the number of added/removed objects, or
/// `None` if no active virtual device with that serial exists.
///
/// # When to use
///
/// Call this after manipulating files in the backing directory directly on
/// disk (outside of MTP) when you need the virtual device to reflect those
/// changes immediately. This avoids waiting for the filesystem watcher's
/// latency (200–500ms on macOS). For delete+recreate sequences, pair with
/// [`pause_watcher`] to prevent stale events from corrupting state.
///
/// # Example
///
/// ```rust,no_run
/// use mtp_rs::rescan_virtual_device;
///
/// // After manipulating files in the backing directory...
/// if let Some(summary) = rescan_virtual_device("my-device-serial") {
///     println!("Rescan: {} added, {} removed", summary.added, summary.removed);
/// }
/// ```
pub fn rescan_virtual_device(serial: &str) -> Option<RescanSummary> {
    let active = active_states().lock().unwrap();
    let state_arc = active
        .iter()
        .find(|(s, _)| s == serial)
        .map(|(_, state)| Arc::clone(state))?;
    drop(active); // Release the registry lock before acquiring the state lock.
    let mut state = state_arc.lock().unwrap();
    Some(state.rescan_backing_dirs())
}

/// Re-key the object at `rel_path` on a virtual device, identified by serial:
/// reassign its object handle while keeping the object (and its on-disk
/// contents) in place. The OLD handle is invalidated (the device answers
/// `InvalidObjectHandle` / `InvalidParentObject` for it); a fresh listing of the
/// parent returns the NEW handle.
///
/// Returns `(old_handle, new_handle)`, or `None` if no active device with that
/// serial exists or no tracked object matches `rel_path` (the object must have
/// been listed at least once so it's tracked).
///
/// # When to use
///
/// To reproduce the **stale cached handle** quirk: Android's MediaProvider
/// re-keys object IDs across a media rescan, so a handle a host cached when it
/// last listed a folder can be silently invalidated before a later operation
/// (upload, delete) into that folder. Unlike [`rescan_virtual_device`] this
/// queues no events: it models the device moving on before the host has
/// observed the change, which is the exact window the bug lives in. Pair it with
/// a list-then-rekey-then-operate sequence to drive a host's stale-handle
/// recovery path.
///
/// # Example
///
/// ```rust,no_run
/// use mtp_rs::rekey_virtual_object;
/// use std::path::Path;
///
/// // The host listed "Documents" earlier and cached its handle. Simulate the
/// // device re-keying it out from under the host:
/// if let Some((old, new)) = rekey_virtual_object("my-device-serial", Path::new("Documents")) {
///     println!("Documents re-keyed: {old:?} -> {new:?}");
/// }
/// ```
pub fn rekey_virtual_object(serial: &str, rel_path: &Path) -> Option<(ObjectHandle, ObjectHandle)> {
    let active = active_states().lock().unwrap();
    let state_arc = active
        .iter()
        .find(|(s, _)| s == serial)
        .map(|(_, state)| Arc::clone(state))?;
    drop(active); // Release the registry lock before acquiring the state lock.
    let mut state = state_arc.lock().unwrap();
    state.rekey_object(rel_path)
}

/// Clamp the lengths of the next `GetPartialObject(64)` reads on this device,
/// one `cap` from `caps` consumed per read (front first).
///
/// Each cap limits how many bytes that read returns below what was requested:
/// `0` makes the read return an **empty** data container (a device stalling
/// mid-file: 0 bytes while bytes remain), `n > 0` a **short** read of `n` real
/// bytes (a legal partial read). Reads past the end of `caps` behave normally.
/// Replaces any previously-queued caps. Returns `false` if no active device with
/// that serial exists.
///
/// # When to use
///
/// To exercise the windowed-download edge cases the virtual device can't produce
/// on its own:
/// - **Stall** (`vec![0]`): a window read returning 0 bytes while the file still
///   has bytes remaining (`offset < size`) must surface an error, not be mistaken
///   for a clean end-of-file and not spun on.
/// - **Short mid-file read** (`vec![n]`): a window read returning fewer bytes than
///   requested mid-file is legal; the download must advance by the bytes actually
///   returned, staying byte-exact.
pub fn force_partial_read_caps(serial: &str, caps: Vec<usize>) -> bool {
    let active = active_states().lock().unwrap();
    let state_arc = match active.iter().find(|(s, _)| s == serial) {
        Some((_, state)) => Arc::clone(state),
        None => return false,
    };
    drop(active); // Release the registry lock before acquiring the state lock.
    state_arc.lock().unwrap().forced_partial_read_caps = caps.into();
    true
}

/// Make `GetObjectInfo` for `handle` answer with a response code instead of an
/// ObjectInfo dataset, while the object stays present and readable by every
/// other operation (test hook).
///
/// Pass `None` for `response_code` to use `GeneralError` (0x2002), the only code
/// the library currently treats as skippable. Returns `false` if no active
/// device has the given serial. Sticky, not one-shot: a second listing of the
/// same folder behaves the same way. Clear it with
/// [`clear_object_info_errors`].
///
/// # When to use
///
/// To reproduce a device that enumerates a handle and then refuses to describe
/// it: the listing knows the folder has N entries but can only read N-1 of them.
/// Sphaira (Nintendo Switch homebrew) does this for one handle out of 50
/// ([#22](https://github.com/vdavid/mtp-rs/issues/22)), and it's the precondition
/// for the whole tolerant-listing contract:
///
/// - [`Storage::list_objects`](crate::Storage::list_objects) returns the readable
///   siblings instead of failing outright.
/// - [`Storage::collect_objects`](crate::Storage::collect_objects) additionally
///   reports the handle and its error.
/// - [`ObjectListing::next`](crate::ObjectListing::next) yields
///   [`ListingItem::Skipped`](crate::ListingItem::Skipped) rather than `Err`.
/// - Marking **every** handle in a folder turns the folder into a hard error
///   rather than an empty listing.
///
/// Without this hook that whole path needs either a Nintendo Switch or a
/// hand-built mock transport, so downstream consumers (file managers, FUSE
/// mounts) had no way to test how they render a partially-readable folder.
pub fn force_object_info_error(
    serial: &str,
    handle: crate::mtp::ObjectHandle,
    response_code: Option<u16>,
) -> bool {
    let active = active_states().lock().unwrap();
    let state_arc = match active.iter().find(|(s, _)| s == serial) {
        Some((_, state)) => Arc::clone(state),
        None => return false,
    };
    drop(active); // Release the registry lock before acquiring the state lock.
    state_arc
        .lock()
        .unwrap()
        .forced_object_info_errors
        .insert(handle.to_ptp().0, response_code.unwrap_or(GENERAL_ERROR));
    true
}

/// Clear every forced `GetObjectInfo` error on this device, so its objects
/// describe themselves normally again. Returns `false` if no active device has
/// the given serial.
pub fn clear_object_info_errors(serial: &str) -> bool {
    let active = active_states().lock().unwrap();
    let state_arc = match active.iter().find(|(s, _)| s == serial) {
        Some((_, state)) => Arc::clone(state),
        None => return false,
    };
    drop(active); // Release the registry lock before acquiring the state lock.
    state_arc.lock().unwrap().forced_object_info_errors.clear();
    true
}

/// Arm a one-shot cancel wedge on a registered virtual device (test hook).
///
/// The next `cancel_transfer` on that device returns
/// [`PtpError::DeviceReset`](crate::PtpError::DeviceReset), modeling the Samsung
/// cancel wedge that the real USB transport detects and recovers
/// from with a device reset (issue #18). This lets the high-level `DeviceReset`
/// contract — a mid-stream `cancel()` surfacing `Error::DeviceReset` so the
/// consumer reopens — be regression-tested with no hardware. Returns `false` if
/// no active device has the given serial.
pub fn force_cancel_wedge(serial: &str) -> bool {
    let active = active_states().lock().unwrap();
    let state_arc = match active.iter().find(|(s, _)| s == serial) {
        Some((_, state)) => Arc::clone(state),
        None => return false,
    };
    drop(active); // Release the registry lock before acquiring the state lock.
    state_arc.lock().unwrap().pending_cancel_wedge = true;
    true
}

/// Arm a one-shot operation wedge on a registered virtual device (test hook).
///
/// The next PTP operation on that device fails with
/// [`PtpError::DeviceReset`](crate::PtpError::DeviceReset), surfacing to the
/// caller as [`Error::DeviceReset`](crate::Error::DeviceReset). Returns `false`
/// if no active device has the given serial.
///
/// # When to use
///
/// [`force_cancel_wedge`] arms the next `cancel_transfer`, so it only fires for
/// a consumer that calls `cancel()`. A consumer that never cancels still reaches
/// `DeviceReset`: it drops an operation future (a superseded listing, a raced
/// timeout), and the next operation's recovery drain hits the wedged device and
/// returns the reset. That's a real path (hardware-verified on a Galaxy S23
/// Ultra SM-S918B, macOS/nusb, 2026-07-20: a dropped mid-flight windowed
/// `GetPartialObject64` produced `DeviceReset` from the drain, and the session
/// was dead afterwards), and this hook is how to test the reopen-and-retry
/// response to it with no hardware.
///
/// The injection point is the operation itself, not the drain, because
/// abandoning a virtual-device future mid-flight is a race (its operations
/// complete instantly). What the consumer observes is the same either way:
/// `Error::DeviceReset` out of an ordinary operation. What it can't observe here
/// is the aftermath: a real device's session is dead until a spaced-retry
/// reopen, while the virtual device is healthy on the very next call.
pub fn force_operation_wedge(serial: &str) -> bool {
    let active = active_states().lock().unwrap();
    let state_arc = match active.iter().find(|(s, _)| s == serial) {
        Some((_, state)) => Arc::clone(state),
        None => return false,
    };
    drop(active); // Release the registry lock before acquiring the state lock.
    state_arc.lock().unwrap().pending_operation_wedge = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::virtual_device::config::VirtualStorageConfig;
    use std::time::Duration;

    fn make_config(serial: &str) -> (VirtualDeviceConfig, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = VirtualDeviceConfig {
            manufacturer: "TestCorp".into(),
            model: "Virtual Phone".into(),
            serial: serial.into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: dir.path().to_path_buf(),
                read_only: false,
            }],
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        };
        (config, dir)
    }

    #[test]
    fn register_and_list() {
        let (config, _dir) = make_config("reg-test-001");
        let info = register_virtual_device(&config);

        assert!(info.location_id >= VIRTUAL_LOCATION_BASE);
        assert_eq!(info.serial_number.as_deref(), Some("reg-test-001"));
        assert_eq!(info.manufacturer.as_deref(), Some("TestCorp"));

        let devices = list_virtual_devices();
        assert!(devices
            .iter()
            .any(|d| d.serial_number.as_deref() == Some("reg-test-001")));

        // Clean up
        unregister_virtual_device(info.location_id);
    }

    #[test]
    fn find_by_location() {
        let (config, _dir) = make_config("reg-test-002");
        let info = register_virtual_device(&config);

        let found = find_virtual_config_by_location(info.location_id);
        assert!(found.is_some());
        assert_eq!(found.unwrap().serial, "reg-test-002");

        // Clean up
        unregister_virtual_device(info.location_id);
    }

    #[test]
    fn find_by_serial() {
        let (config, _dir) = make_config("reg-test-003");
        let info = register_virtual_device(&config);

        let found = find_virtual_config_by_serial("reg-test-003");
        assert!(found.is_some());
        assert_eq!(found.unwrap().serial, "reg-test-003");

        // Not found
        assert!(find_virtual_config_by_serial("nonexistent").is_none());

        // Clean up
        unregister_virtual_device(info.location_id);
    }

    #[test]
    fn unregister() {
        let (config, _dir) = make_config("reg-test-004");
        let info = register_virtual_device(&config);

        unregister_virtual_device(info.location_id);

        assert!(find_virtual_config_by_location(info.location_id).is_none());
        assert!(find_virtual_config_by_serial("reg-test-004").is_none());
    }

    #[test]
    fn location_id_unique_after_unregister() {
        let (config_a, _dir_a) = make_config("reg-test-unique-a");
        let info_a = register_virtual_device(&config_a);

        let (config_b, _dir_b) = make_config("reg-test-unique-b");
        let info_b = register_virtual_device(&config_b);

        // Unregister A
        unregister_virtual_device(info_a.location_id);

        // Register C: must get a unique location_id different from both A and B
        let (config_c, _dir_c) = make_config("reg-test-unique-c");
        let info_c = register_virtual_device(&config_c);

        assert_ne!(info_c.location_id, info_a.location_id);
        assert_ne!(info_c.location_id, info_b.location_id);

        // Clean up
        unregister_virtual_device(info_b.location_id);
        unregister_virtual_device(info_c.location_id);
    }

    #[tokio::test]
    async fn open_by_location_integration() {
        let dir = tempfile::tempdir().unwrap();
        let config = VirtualDeviceConfig {
            model: "Registry Phone".into(),
            serial: "reg-test-005".into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: dir.path().to_path_buf(),
                read_only: false,
            }],
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        };
        let info = register_virtual_device(&config);

        let device = crate::MtpDevice::builder()
            .open_by_location(info.location_id)
            .await
            .unwrap();
        assert_eq!(device.device_info().model, "Registry Phone");

        // Clean up
        unregister_virtual_device(info.location_id);
    }

    #[tokio::test]
    async fn open_by_serial_integration() {
        let dir = tempfile::tempdir().unwrap();
        let config = VirtualDeviceConfig {
            model: "Registry Phone".into(),
            serial: "reg-test-006".into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: dir.path().to_path_buf(),
                read_only: false,
            }],
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        };
        let info = register_virtual_device(&config);

        let device = crate::MtpDevice::builder()
            .open_by_serial("reg-test-006")
            .await
            .unwrap();
        assert_eq!(device.device_info().model, "Registry Phone");

        // Clean up
        unregister_virtual_device(info.location_id);
    }

    // ── Pause refcount + dropped-paths tracking ─────────────────────────────
    //
    // The state lookups below go through `active_states()`, which is the
    // OPENED-device registry (populated by `VirtualTransport::new`), not the
    // discovery registry that `register_virtual_device` writes to. So these
    // tests open a real `MtpDevice` (via `#[tokio::test]` + `open_by_serial`)
    // and let it `Drop` to unregister via `VirtualTransport::drop`.

    async fn open_test_device(serial: &str) -> (crate::MtpDevice, MtpDeviceInfo) {
        let dir = tempfile::tempdir().unwrap();
        // Release the tempdir's auto-cleanup guard so the path stays valid for
        // the device's lifetime; /tmp clears on reboot, which is fine for test
        // fixtures.
        let backing = dir.keep();
        let config = VirtualDeviceConfig {
            model: "Drain Phone".into(),
            serial: serial.into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: backing,
                read_only: false,
            }],
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        };
        let info = register_virtual_device(&config);
        let device = crate::MtpDevice::builder()
            .open_by_serial(serial)
            .await
            .unwrap();
        (device, info)
    }

    fn state_of(serial: &str) -> Arc<Mutex<VirtualDeviceState>> {
        let active = active_states().lock().unwrap();
        active
            .iter()
            .find(|(s, _)| s == serial)
            .map(|(_, s)| Arc::clone(s))
            .expect("device must be opened (not just registered) before state lookup")
    }

    #[tokio::test]
    async fn pause_refcount_composes_across_concurrent_guards() {
        let (device, info) = open_test_device("pause-refcount-001").await;

        let guard_a = pause_watcher("pause-refcount-001").expect("device is open");
        assert_eq!(
            state_of("pause-refcount-001").lock().unwrap().pause_count,
            1
        );

        let guard_b = pause_watcher("pause-refcount-001").expect("still open");
        assert_eq!(
            state_of("pause-refcount-001").lock().unwrap().pause_count,
            2
        );

        // First drop must leave the watcher paused: the other guard is alive.
        drop(guard_a);
        assert_eq!(
            state_of("pause-refcount-001").lock().unwrap().pause_count,
            1
        );

        // Last drop releases.
        drop(guard_b);
        assert_eq!(
            state_of("pause-refcount-001").lock().unwrap().pause_count,
            0
        );

        drop(device); // VirtualTransport::drop unregisters active state
        unregister_virtual_device(info.location_id);
    }

    #[test]
    fn pause_watcher_returns_none_for_unknown_serial() {
        // Nothing to open; just hit the lookup miss path.
        assert!(pause_watcher("pause-refcount-no-such-serial").is_none());
    }

    #[tokio::test]
    async fn dropped_paths_observation_round_trip() {
        let (device, info) = open_test_device("dropped-paths-001").await;

        // Empty before anything pushed.
        assert!(dropped_paths_since_pause("dropped-paths-001").is_empty());
        assert!(!was_path_dropped("dropped-paths-001", "sentinel-xyz"));

        // Push two paths directly (this unit tests the observation API; the
        // watcher's own dropped-paths push path is covered by the downstream
        // virtual-mtp E2E suite, where the OS actually delivers FS events).
        {
            let state_arc = state_of("dropped-paths-001");
            let mut state = state_arc.lock().unwrap();
            state
                .dropped_paths
                .push_back(PathBuf::from("/tmp/cmdr-mtp/internal/foo.txt"));
            state
                .dropped_paths
                .push_back(PathBuf::from("/tmp/cmdr-mtp/internal/sentinel-xyz"));
        }

        // Primary primitive returns both, oldest first.
        let paths = dropped_paths_since_pause("dropped-paths-001");
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("/tmp/cmdr-mtp/internal/foo.txt"));

        // Suffix convenience matches.
        assert!(was_path_dropped("dropped-paths-001", "sentinel-xyz"));
        assert!(was_path_dropped("dropped-paths-001", "/foo.txt")); // suffix, not basename
        assert!(!was_path_dropped("dropped-paths-001", "not-there"));

        // Clear empties the ring without affecting the device's other state.
        clear_dropped_paths("dropped-paths-001");
        assert!(dropped_paths_since_pause("dropped-paths-001").is_empty());

        drop(device);
        unregister_virtual_device(info.location_id);
    }

    #[test]
    fn dropped_paths_for_unknown_serial_returns_empty() {
        // Defensively returns Vec::new() / false instead of panicking, so
        // polling loops don't have to special-case device unregistration.
        assert!(dropped_paths_since_pause("dropped-paths-no-such-serial").is_empty());
        assert!(!was_path_dropped(
            "dropped-paths-no-such-serial",
            "anything"
        ));
        clear_dropped_paths("dropped-paths-no-such-serial"); // no-op, must not panic
    }

    #[tokio::test]
    async fn dropped_paths_ring_evicts_oldest_past_cap() {
        use crate::transport::virtual_device::state::DROPPED_PATHS_CAP;

        let (device, info) = open_test_device("dropped-paths-cap-001").await;

        // Push cap + 5 entries directly. Oldest 5 must evict; newest 5 must
        // remain at the back.
        {
            let state_arc = state_of("dropped-paths-cap-001");
            let mut state = state_arc.lock().unwrap();
            for i in 0..(DROPPED_PATHS_CAP + 5) {
                state
                    .dropped_paths
                    .push_back(PathBuf::from(format!("/tmp/drop-{i}")));
                if state.dropped_paths.len() > DROPPED_PATHS_CAP {
                    state.dropped_paths.pop_front();
                }
            }
        }

        let paths = dropped_paths_since_pause("dropped-paths-cap-001");
        assert_eq!(paths.len(), DROPPED_PATHS_CAP);
        // The first 5 (indices 0..5) were evicted; oldest surviving is index 5.
        assert_eq!(paths[0], PathBuf::from("/tmp/drop-5"));
        assert_eq!(
            paths[DROPPED_PATHS_CAP - 1],
            PathBuf::from(format!("/tmp/drop-{}", DROPPED_PATHS_CAP + 4))
        );

        drop(device);
        unregister_virtual_device(info.location_id);
    }
}
