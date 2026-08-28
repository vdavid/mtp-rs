//! MtpDevice - the main entry point for MTP operations.

use crate::mtp::backend::usb::UsbBackend;
use crate::mtp::backend::{Backend, MtpBackend};
use crate::mtp::{Capabilities, DeviceEvent, DeviceInfo, Error, Storage, StorageId};
use crate::ptp::PtpSession;
use crate::transport::{NusbTransport, Transport};
use std::sync::Arc;
use std::time::Duration;

/// An MTP device connection.
///
/// This is the main entry point for interacting with MTP devices.
/// Use `MtpDevice::open_first()` to connect to the first available device,
/// or `MtpDevice::builder()` for more control.
///
/// The device is a thin façade over a backend-neutral implementation (the internal `MtpBackend`
/// trait). Today the only backend is PTP-over-USB (which also drives the virtual and mock
/// transports); a Windows WPD backend is planned. Consumers work against the neutral
/// [`crate::mtp`] types throughout.
///
/// # Example
///
/// ```rust,no_run
/// use mtp_rs::mtp::MtpDevice;
///
/// # async fn example() -> Result<(), mtp_rs::Error> {
/// // Open the first MTP device
/// let device = MtpDevice::open_first().await?;
///
/// println!("Connected to: {} {}",
///          device.device_info().manufacturer,
///          device.device_info().model);
///
/// // Get storages
/// for storage in device.storages().await? {
///     println!("Storage: {} ({} free)",
///              storage.info().description,
///              storage.info().free_space);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct MtpDevice {
    pub(crate) backend: Arc<dyn MtpBackend>,
}

impl MtpDevice {
    /// Create a builder for configuring device options.
    pub fn builder() -> MtpDeviceBuilder {
        MtpDeviceBuilder::new()
    }

    /// Open the first available MTP device with default settings.
    pub async fn open_first() -> Result<Self, Error> {
        Self::builder().open_first().await
    }

    /// Open a device at a specific USB location (port) with default settings.
    ///
    /// Use `list_devices()` to get available location IDs.
    pub async fn open_by_location(location_id: u64) -> Result<Self, Error> {
        Self::builder().open_by_location(location_id).await
    }

    /// Open a device by its serial number with default settings.
    ///
    /// This identifies a specific physical device regardless of which USB port
    /// it's connected to.
    pub async fn open_by_serial(serial: &str) -> Result<Self, Error> {
        Self::builder().open_by_serial(serial).await
    }

    /// Reset the USB transport state of the device with this serial, without
    /// opening a session.
    ///
    /// Sends the USB Still Image Class Device Reset request (`bRequest=0x66`),
    /// clears halted bulk endpoints, and drains stale bulk data. This is the USB
    /// **transport-level** reset, not the in-session `ResetDevice` (0x1010) PTP
    /// operation: it works precisely when the device is too confused for PTP
    /// traffic, which is when you need it.
    ///
    /// # Warning: on Android this can break MTP until the user replugs
    ///
    /// **Treat this as a last resort, not a recovery step.** Sending the reset to
    /// a *healthy* Pixel 9 Pro XL permanently killed its MTP function: Android's
    /// `MtpServer` lost its endpoint read (`ECANCELED`, then `EPIPE`) and never
    /// re-armed, while the USB device controller stayed `configured`. The phone
    /// kept enumerating and kept showing up in a device list, and answered
    /// nothing. Ten spaced reopens over ~100 s all timed out; only a physical
    /// unplug and replug brought it back (verified on a Pixel 9 Pro XL,
    /// macOS/nusb + `adb logcat`, 2026-07-21).
    ///
    /// Android is the most common MTP device class, so reach for this only after
    /// spaced reopens have already failed, or on a device that's *already*
    /// unreachable, where you can't make things much worse. See
    /// `docs/notes/android-wedges-and-the-reset-kill-switch.md`.
    ///
    /// # Why this isn't a method on an open device
    ///
    /// It only claims the USB interface and stops there. The regular opens run
    /// `OpenSession` + `GetDeviceInfo`, which is exactly what a wedged device
    /// can't answer, so a reset hanging off an already-open [`MtpDevice`] would
    /// be useless in the case it exists for. **Drop your device first**: holding
    /// it keeps the interface claimed, and this call would then fail to claim it.
    /// You have to reopen afterwards regardless, since the PTP session is gone.
    ///
    /// # Recovering a wedged device
    ///
    /// After [`Error::DeviceReset`], when an operation hangs and never returns
    /// (the Android signature: no error at all), or when every operation fails
    /// with "Transaction ID mismatch" / "expected Response container type":
    ///
    /// 1. Drop the [`MtpDevice`] (and any [`Storage`] handles).
    /// 2. Wait a few seconds **quiet**, with no USB traffic at all.
    /// 3. Reopen with idle-spaced retries, several of them.
    /// 4. Only if every reopen failed, and knowing the Android warning above,
    ///    call this and then repeat steps 2 and 3.
    ///
    /// Step 3 is where consumers go wrong: don't try once and give up, and don't
    /// hammer close/open in a tight loop (that keeps the device busy and
    /// re-wedges it into a hard `Timeout`). Expect the early attempts to fail.
    /// A Pixel's wedge cleared on a fresh open with no reset at all (verified on
    /// a Pixel 9 Pro XL, macOS/nusb, 2026-07-20). On a Galaxy S23 Ultra the
    /// observed sequence was reset, then a reopen returning `Timeout`, then one
    /// returning `SessionAlreadyOpen`, then success (verified on SM-S918B,
    /// macOS/nusb, 2026-07-20); the control without a reset was never run, so
    /// it's unknown whether spaced reopens alone would have sufficed there.
    ///
    /// # Errors
    ///
    /// [`Error::NoDevice`] when no USB device has that serial, and
    /// [`Error::Unsupported`] for a virtual device, which is a filesystem with no
    /// USB transport to reset.
    pub async fn reset_by_serial(serial: &str) -> Result<(), Error> {
        Self::builder().reset_by_serial(serial).await
    }

    /// Reset the USB transport state of the device at this location, without
    /// opening a session.
    ///
    /// See [`reset_by_serial`](Self::reset_by_serial) for the full contract and
    /// the recovery sequence to follow.
    ///
    /// **Last resort on Android**: the reset can break the phone's MTP function
    /// until the user physically replugs. Try spaced reopens first.
    pub async fn reset_by_location(location_id: u64) -> Result<(), Error> {
        Self::builder().reset_by_location(location_id).await
    }

    /// Reset the USB transport state of the first available device, without
    /// opening a session.
    ///
    /// See [`reset_by_serial`](Self::reset_by_serial) for the full contract and
    /// the recovery sequence to follow.
    ///
    /// **Last resort on Android**: the reset can break the phone's MTP function
    /// until the user physically replugs. Try spaced reopens first.
    pub async fn reset_first() -> Result<(), Error> {
        Self::builder().reset_first().await
    }

    /// List all available MTP devices without opening them.
    pub fn list_devices() -> Result<Vec<MtpDeviceInfo>, Error> {
        Self::list_devices_with_known(&[])
    }

    /// List all available MTP devices, including additional devices identified
    /// by the given VID/PID pairs.
    ///
    /// Devices matching the provided VID/PID pairs are included in the results
    /// even if their USB descriptors don't match standard MTP class codes. This
    /// is useful for legacy or otherwise unusual devices with non-standard USB
    /// descriptors that still speak MTP.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mtp_rs::mtp::MtpDevice;
    ///
    /// let devices = MtpDevice::list_devices_with_known(&[
    ///     (0x045E, 0x0710), // custom VID/PID
    /// ])?;
    /// for d in &devices {
    ///     println!("{:04x}:{:04x} {}", d.vendor_id, d.product_id,
    ///              d.product.as_deref().unwrap_or("unknown"));
    /// }
    /// # Ok::<(), mtp_rs::Error>(())
    /// ```
    pub fn list_devices_with_known(known: &[(u16, u16)]) -> Result<Vec<MtpDeviceInfo>, Error> {
        let devices = NusbTransport::list_mtp_devices_with_known(known)?;
        #[allow(unused_mut)]
        let mut result: Vec<MtpDeviceInfo> =
            devices.into_iter().map(MtpDeviceInfo::from_usb).collect();

        #[cfg(feature = "virtual-device")]
        result.extend(crate::transport::virtual_device::registry::list_virtual_devices());

        Ok(result)
    }

    /// Get device information (backend-neutral identity).
    #[must_use]
    pub fn device_info(&self) -> &DeviceInfo {
        self.backend.device_info()
    }

    /// What this device supports (backend-neutral capabilities).
    ///
    /// Replaces the old per-operation accessors. Advertised support can still be wrong on some
    /// devices (see the Fujifilm quirk in `AGENTS.md`), so treat these as a strong hint.
    #[must_use]
    pub fn capabilities(&self) -> &Capabilities {
        self.backend.capabilities()
    }

    /// Whether the device supports renaming objects.
    ///
    /// Convenience over [`capabilities()`](Self::capabilities)`.can_rename`.
    #[must_use]
    pub fn supports_rename(&self) -> bool {
        self.backend.capabilities().can_rename
    }

    /// Whether the device supports creating objects (uploads and folders).
    ///
    /// Convenience over [`capabilities()`](Self::capabilities)`.can_upload`.
    #[must_use]
    pub fn supports_upload(&self) -> bool {
        self.backend.capabilities().can_upload
    }

    /// Get all storages on the device.
    pub async fn storages(&self) -> Result<Vec<Storage>, Error> {
        let infos = self.backend.storages().await?;
        Ok(infos
            .into_iter()
            .map(|info| Storage::new(Arc::clone(&self.backend), info.id, info))
            .collect())
    }

    /// Get a specific storage by ID.
    pub async fn storage(&self, id: StorageId) -> Result<Storage, Error> {
        let info = self.backend.storage_info(id).await?;
        Ok(Storage::new(Arc::clone(&self.backend), id, info))
    }

    /// Receive the next event from the device.
    ///
    /// This method awaits **indefinitely** on the underlying event channel until an
    /// event arrives or the device disconnects. Always wrap this in
    /// `tokio::time::timeout` (or equivalent) so you can check for shutdown.
    ///
    /// # Concurrency
    ///
    /// On the USB backend, event reading uses the USB interrupt endpoint, which is
    /// independent from the bulk endpoints used by file operations, so it is safe to
    /// call `next_event()` concurrently with other `MtpDevice` methods.
    ///
    /// If you wrap `MtpDevice` in a shared lock (for example, `Arc<Mutex<MtpDevice>>`),
    /// do **not** hold that lock while awaiting `next_event()`: it will block all file
    /// operations for the duration of the wait. Instead, clone the `MtpDevice` (it is
    /// cheaply cloneable via `Arc` internally) and call `next_event()` on the clone
    /// without holding the lock.
    ///
    /// # Returns
    ///
    /// - `Ok(event)` - An event was received from the device
    /// - `Err(Error::Disconnected)` - Device was disconnected
    /// - `Err(_)` - Other communication error
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mtp_rs::mtp::{MtpDevice, DeviceEvent};
    /// use mtp_rs::Error;
    /// use tokio::time::{timeout, Duration};
    ///
    /// # async fn example() -> Result<(), Error> {
    /// # let device = MtpDevice::open_first().await?;
    /// loop {
    ///     match timeout(Duration::from_millis(200), device.next_event()).await {
    ///         Ok(Ok(event)) => {
    ///             match event {
    ///                 DeviceEvent::ObjectAdded { handle } => {
    ///                     println!("New object: {:?}", handle);
    ///                 }
    ///                 DeviceEvent::StoreRemoved { storage_id } => {
    ///                     println!("Storage removed: {:?}", storage_id);
    ///                 }
    ///                 _ => {}
    ///             }
    ///         }
    ///         Ok(Err(Error::Disconnected)) => break,
    ///         Ok(Err(e)) => {
    ///             eprintln!("Error: {}", e);
    ///             break;
    ///         }
    ///         Err(_elapsed) => continue,  // Timeout, check for shutdown etc.
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn next_event(&self) -> Result<DeviceEvent, Error> {
        self.backend.next_event().await
    }

    /// Explicitly close the connection.
    ///
    /// On the USB backend this sends a best-effort `CloseSession`; dropping [`MtpDevice`] does not.
    /// This still applies after [`MtpDeviceBuilder::reuse_existing_session`]. On devices such as the
    /// Teenage Engineering TP-7, an explicit close makes the device leave MTP mode.
    pub async fn close(self) -> Result<(), Error> {
        self.backend.close().await
    }
}

/// Information about an MTP device (without opening it).
///
/// This struct provides device identification at multiple levels:
///
/// - **Device identity** (`vendor_id`, `product_id`, `serial_number`): Identifies
///   a specific physical device. Use this to recognize "John's phone" regardless
///   of which USB port it's plugged into.
///
/// - **Port identity** (`location_id`): Identifies the physical USB port/location.
///   Use this when you care about "the device on port 3" rather than which
///   specific device it is. Stable across reconnections to the same port.
///
/// - **Display info** (`manufacturer`, `product`): Human-readable strings for
///   showing device info to users.
///
/// # Example
///
/// ```rust,no_run
/// use mtp_rs::mtp::MtpDevice;
///
/// let devices = MtpDevice::list_devices()?;
/// for dev in &devices {
///     println!("{} {} (serial: {:?})",
///              dev.manufacturer.as_deref().unwrap_or("Unknown"),
///              dev.product.as_deref().unwrap_or("Unknown"),
///              dev.serial_number);
/// }
///
/// // Save location_id to remember "the device on this port"
/// // Save serial_number to remember "this specific phone"
/// # Ok::<(), mtp_rs::Error>(())
/// ```
///
/// Marked `#[non_exhaustive]` so future field additions don't break consumers
/// that pattern-match or destructure. Construct via [`MtpDevice::list_devices`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MtpDeviceInfo {
    /// USB vendor ID (assigned by USB-IF to each company).
    ///
    /// Examples: Google = `0x18d1`, Samsung = `0x04e8`, Apple = `0x05ac`
    pub vendor_id: u16,

    /// USB product ID (assigned by vendor to each product model).
    ///
    /// Note: The same device may report different product IDs depending on
    /// its USB mode (MTP, ADB, charging-only, etc.).
    pub product_id: u16,

    /// Manufacturer name from USB descriptor.
    ///
    /// Examples: `"Google"`, `"Samsung"`, `"Apple Inc."`
    ///
    /// `None` if the device doesn't report a manufacturer string.
    pub manufacturer: Option<String>,

    /// Product name from USB descriptor.
    ///
    /// Examples: `"Pixel 9 Pro XL"`, `"Galaxy S24"`
    ///
    /// `None` if the device doesn't report a product string.
    pub product: Option<String>,

    /// Serial number uniquely identifying this specific device.
    ///
    /// Combined with `vendor_id` and `product_id`, this globally identifies
    /// a single physical device. Survives reconnection to different ports.
    ///
    /// `None` if the device doesn't report a serial number.
    pub serial_number: Option<String>,

    /// Physical USB location identifier.
    ///
    /// Identifies the USB port/path where the device is connected. Stable
    /// across reconnections to the same physical port, but changes if the
    /// device is moved to a different port.
    ///
    /// Derived cross-platform from the USB bus ID and port chain (topology).
    pub location_id: u64,

    /// Negotiated USB link speed (slowest of host port, cable, and device).
    ///
    /// A USB 3.2 Gen 2 phone connected through a USB 2.0 charging cable
    /// reports `High` (480 Mbit/s), not the device's capability.
    ///
    /// `None` if the OS doesn't report it for this device.
    pub speed: Option<crate::transport::UsbSpeed>,

    /// Why this USB device was classified as an MTP candidate.
    pub match_reason: crate::transport::MtpMatchReason,
}

impl MtpDeviceInfo {
    /// Build the neutral info from a USB-transport listing entry.
    pub(crate) fn from_usb(d: crate::transport::UsbDeviceInfo) -> Self {
        Self {
            vendor_id: d.vendor_id,
            product_id: d.product_id,
            manufacturer: d.manufacturer,
            product: d.product,
            serial_number: d.serial_number,
            location_id: d.location_id,
            speed: d.speed,
            match_reason: d.match_reason,
        }
    }

    /// Format the device info for display.
    #[must_use]
    pub fn display(&self) -> String {
        let manufacturer = self.manufacturer.as_deref().unwrap_or("Unknown");
        let product = self.product.as_deref().unwrap_or("Unknown");
        match &self.serial_number {
            Some(serial) => format!(
                "{} {} (serial: {}, location: {:08x})",
                manufacturer, product, serial, self.location_id
            ),
            None => format!(
                "{} {} (location: {:08x})",
                manufacturer, product, self.location_id
            ),
        }
    }
}

/// Builder for MtpDevice configuration.
pub struct MtpDeviceBuilder {
    timeout: Duration,
    known_devices: Vec<(u16, u16)>,
    backend: Backend,
    reuse_existing_session_id: Option<u32>,
}

impl MtpDeviceBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            timeout: NusbTransport::DEFAULT_TIMEOUT,
            known_devices: Vec::new(),
            backend: Backend::default(),
            reuse_existing_session_id: None,
        }
    }

    /// Choose which backend to open (default [`Backend::Auto`]).
    ///
    /// On Windows, `Auto` prefers WPD (for phones) and falls back to USB; pass [`Backend::Usb`] to
    /// force PTP-over-USB to a Zadig/WinUSB-bound camera, or [`Backend::Wpd`] to force WPD.
    #[must_use]
    pub fn backend(mut self, backend: Backend) -> Self {
        self.backend = backend;
        self
    }

    #[cfg(windows)]
    fn diagnose_reuse_ignored_by_wpd(&self) {
        if let Some(session_id) = self.reuse_existing_session_id {
            diag_debug!(
                "reuse_existing_session({}) ignored because the selected WPD backend does not use PTP sessions",
                session_id
            );
        }
    }

    /// If the configured backend selects WPD (Windows), try to open the first WPD device.
    ///
    /// Returns `Ok(None)` when WPD isn't selected, or when `Auto` found no WPD device (so the caller
    /// falls back to USB). A forced [`Backend::Wpd`], or any non-"no device" WPD error, propagates.
    async fn try_open_wpd_first(&self) -> Result<Option<MtpDevice>, Error> {
        #[cfg(windows)]
        if matches!(self.backend, Backend::Auto | Backend::Wpd) {
            match crate::mtp::backend::wpd::WpdBackend::open_first().await {
                Ok(b) => {
                    self.diagnose_reuse_ignored_by_wpd();
                    return Ok(Some(MtpDevice {
                        backend: Arc::new(b),
                    }));
                }
                Err(e) if self.backend == Backend::Wpd || !matches!(e, Error::NoDevice) => {
                    return Err(e)
                }
                Err(_) => {} // Auto + no WPD device: fall back to USB.
            }
        }
        #[cfg(not(windows))]
        if self.backend == Backend::Wpd {
            return Err(Error::Unsupported);
        }
        Ok(None)
    }

    /// As [`try_open_wpd_first`](Self::try_open_wpd_first) but matching a serial number.
    async fn try_open_wpd_by_serial(&self, serial: &str) -> Result<Option<MtpDevice>, Error> {
        #[cfg(windows)]
        if matches!(self.backend, Backend::Auto | Backend::Wpd) {
            match crate::mtp::backend::wpd::WpdBackend::open_by_serial(serial).await {
                Ok(b) => {
                    self.diagnose_reuse_ignored_by_wpd();
                    return Ok(Some(MtpDevice {
                        backend: Arc::new(b),
                    }));
                }
                Err(e) if self.backend == Backend::Wpd || !matches!(e, Error::NoDevice) => {
                    return Err(e)
                }
                Err(_) => {}
            }
        }
        #[cfg(not(windows))]
        {
            let _ = serial;
            if self.backend == Backend::Wpd {
                return Err(Error::Unsupported);
            }
        }
        Ok(None)
    }

    /// As [`try_open_wpd_by_serial`](Self::try_open_wpd_by_serial) but for a USB device (VID/PID plus
    /// the USB descriptor serial), used to correlate an nusb `location_id` to a WPD device.
    ///
    /// The nusb and WPD *device* serials can differ (the Pixel's USB-descriptor serial isn't its WPD
    /// serial), so the WPD side matches on VID/PID and, only when two identical models share it,
    /// disambiguates by the USB serial resolved from the device tree.
    async fn try_open_wpd_for_usb(
        &self,
        serial: Option<String>,
        vid: u16,
        pid: u16,
    ) -> Result<Option<MtpDevice>, Error> {
        #[cfg(windows)]
        if matches!(self.backend, Backend::Auto | Backend::Wpd) {
            match crate::mtp::backend::wpd::WpdBackend::open_for_usb(serial.clone(), vid, pid).await
            {
                Ok(b) => {
                    self.diagnose_reuse_ignored_by_wpd();
                    return Ok(Some(MtpDevice {
                        backend: Arc::new(b),
                    }));
                }
                Err(e) if self.backend == Backend::Wpd || !matches!(e, Error::NoDevice) => {
                    return Err(e)
                }
                Err(_) => {}
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (serial, vid, pid);
            if self.backend == Backend::Wpd {
                return Err(Error::Unsupported);
            }
        }
        Ok(None)
    }

    /// Set bulk transfer timeout (default: 30 seconds).
    ///
    /// This timeout applies to file transfers, command responses, and event polling.
    /// Use longer timeouts for large file operations.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Include additional devices identified by VID/PID pairs in the open-time
    /// device scan.
    ///
    /// By default, the `open_first` / `open_by_serial` / `open_by_location`
    /// convenience methods only consider devices whose USB descriptors match
    /// standard MTP class codes. Pass extra VID/PID pairs here to also accept
    /// legacy or otherwise unusual devices that speak MTP despite reporting
    /// non-standard descriptors.
    ///
    /// This is the open-side counterpart to [`MtpDevice::list_devices_with_known`]:
    /// pair them with the same list to enumerate and open the same set of devices.
    #[must_use]
    pub fn known_devices(mut self, known: &[(u16, u16)]) -> Self {
        self.known_devices = known.to_vec();
        self
    }

    /// Reuse a device-side PTP session with the given stable ID when it is already open.
    ///
    /// The default open behavior closes an existing session and starts a fresh one. This opt-in is
    /// for USB devices that persist a session across host processes and accept the transaction
    /// counter restarting from one. It prevents the open path from sending `CloseSession` after a
    /// `SessionAlreadyOpen` response.
    ///
    /// This setting applies only to the USB backend. It is ignored if [`Backend::Auto`] selects WPD
    /// on Windows, or when [`Backend::Wpd`] is selected explicitly. Calling [`MtpDevice::close`]
    /// still sends `CloseSession` on USB; drop the device instead when its session must remain open.
    #[must_use]
    pub fn reuse_existing_session(mut self, session_id: u32) -> Self {
        self.reuse_existing_session_id = Some(session_id);
        self
    }

    async fn open_ptp_session(
        &self,
        transport: Arc<dyn Transport>,
    ) -> Result<PtpSession, crate::PtpError> {
        match self.reuse_existing_session_id {
            Some(session_id) => PtpSession::open_reusing_existing(transport, session_id).await,
            None => PtpSession::open(transport, 1).await,
        }
    }

    /// Open the first available device.
    pub async fn open_first(self) -> Result<MtpDevice, Error> {
        if let Some(device) = self.try_open_wpd_first().await? {
            return Ok(device);
        }
        let devices = NusbTransport::list_mtp_devices_with_known(&self.known_devices)?;
        let device_info = devices
            .into_iter()
            .next()
            .ok_or(crate::PtpError::NoDevice)?;
        let device = device_info.open().map_err(crate::PtpError::Usb)?;
        self.open_nusb_device(device).await
    }

    /// Open a device at a specific USB location (port).
    ///
    /// Use `MtpDevice::list_devices()` to get available location IDs.
    /// Also checks the virtual device registry when the `virtual-device` feature is enabled.
    ///
    /// On Windows a phone at the location is bound to the WPD driver and can't be claimed over raw
    /// USB, so this correlates the location to a WPD device and opens it there (for
    /// [`Backend::Auto`]/[`Backend::Wpd`]); it falls back to raw USB for WinUSB-bound cameras and on
    /// other platforms. The correlation is by **VID/PID** (the USB-descriptor serial and the WPD
    /// serial can differ), so with two attached devices of the *same model* it may open the other
    /// one — address those by serial instead.
    pub async fn open_by_location(self, location_id: u64) -> Result<MtpDevice, Error> {
        #[cfg(feature = "virtual-device")]
        if let Some(config) =
            crate::transport::virtual_device::registry::find_virtual_config_by_location(location_id)
        {
            return self.open_virtual(config).await;
        }

        let devices = NusbTransport::list_mtp_devices_with_known(&self.known_devices)?;
        let device_info = devices
            .into_iter()
            .find(|d| d.location_id == location_id)
            .ok_or(crate::PtpError::NoDevice)?;

        if let Some(device) = self
            .try_open_wpd_for_usb(
                device_info.serial_number.clone(),
                device_info.vendor_id,
                device_info.product_id,
            )
            .await?
        {
            return Ok(device);
        }

        let device = device_info.open().map_err(crate::PtpError::Usb)?;
        self.open_nusb_device(device).await
    }

    /// Open a device by its serial number.
    ///
    /// This identifies a specific physical device regardless of which USB port
    /// it's connected to. Also checks the virtual device registry when the
    /// `virtual-device` feature is enabled.
    pub async fn open_by_serial(self, serial: &str) -> Result<MtpDevice, Error> {
        #[cfg(feature = "virtual-device")]
        if let Some(config) =
            crate::transport::virtual_device::registry::find_virtual_config_by_serial(serial)
        {
            return self.open_virtual(config).await;
        }

        if let Some(device) = self.try_open_wpd_by_serial(serial).await? {
            return Ok(device);
        }

        let devices = NusbTransport::list_mtp_devices_with_known(&self.known_devices)?;
        let device_info = devices
            .into_iter()
            .find(|d| d.serial_number.as_deref() == Some(serial))
            .ok_or(crate::PtpError::NoDevice)?;
        let device = device_info.open().map_err(crate::PtpError::Usb)?;
        self.open_nusb_device(device).await
    }

    /// Open an already-acquired [`nusb::Device`] as an MTP device.
    ///
    /// This is an escape hatch for consumers who already hold an `nusb::Device`
    /// (e.g. from a custom enumeration or hotplug watcher). For most callers,
    /// prefer [`known_devices`](Self::known_devices) combined with
    /// `open_by_serial` / `open_by_location`.
    ///
    /// The interface scan is permissive: strict MTP-class match first, then
    /// fallback to any interface with the MTP endpoint layout.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mtp_rs::mtp::MtpDevice;
    /// use nusb::MaybeFuture;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let nusb_device = nusb::list_devices()
    ///     .wait()?
    ///     .find(|d: &nusb::DeviceInfo| {
    ///         d.vendor_id() == 0x045E && d.product_id() == 0x0710
    ///     })
    ///     .ok_or(mtp_rs::Error::NoDevice)?
    ///     .open()
    ///     .wait()?;
    ///
    /// let device = MtpDevice::builder()
    ///     .open_nusb_device(nusb_device)
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open_nusb_device(self, device: nusb::Device) -> Result<MtpDevice, Error> {
        let transport = NusbTransport::open_with_timeout(device, self.timeout).await?;
        let transport: Arc<dyn Transport> = Arc::new(transport);

        let session = Arc::new(self.open_ptp_session(transport.clone()).await?);

        // Get device info
        let device_info = session.get_device_info().await?;

        // Quirk for Garmin devices
        if device_info.manufacturer == "Garmin" {
            session.set_split_header_data(true);
        }

        let backend = UsbBackend::new(session, device_info);
        Ok(MtpDevice {
            backend: Arc::new(backend),
        })
    }

    /// Reset the USB transport of the device with this serial, without opening a
    /// session. See [`MtpDevice::reset_by_serial`] for the full contract.
    ///
    /// **Last resort on Android**: the reset can break the phone's MTP function
    /// until the user physically replugs. Try spaced reopens first.
    pub async fn reset_by_serial(self, serial: &str) -> Result<(), Error> {
        #[cfg(feature = "virtual-device")]
        if crate::transport::virtual_device::registry::find_virtual_config_by_serial(serial)
            .is_some()
        {
            return Err(Error::Unsupported);
        }

        let devices = NusbTransport::list_mtp_devices_with_known(&self.known_devices)?;
        let device_info = devices
            .into_iter()
            .find(|d| d.serial_number.as_deref() == Some(serial))
            .ok_or(crate::PtpError::NoDevice)?;
        self.reset_usb_device(device_info).await
    }

    /// Reset the USB transport of the device at this location, without opening a
    /// session. See [`MtpDevice::reset_by_serial`] for the full contract.
    ///
    /// **Last resort on Android**: the reset can break the phone's MTP function
    /// until the user physically replugs. Try spaced reopens first.
    pub async fn reset_by_location(self, location_id: u64) -> Result<(), Error> {
        #[cfg(feature = "virtual-device")]
        if crate::transport::virtual_device::registry::find_virtual_config_by_location(location_id)
            .is_some()
        {
            return Err(Error::Unsupported);
        }

        let devices = NusbTransport::list_mtp_devices_with_known(&self.known_devices)?;
        let device_info = devices
            .into_iter()
            .find(|d| d.location_id == location_id)
            .ok_or(crate::PtpError::NoDevice)?;
        self.reset_usb_device(device_info).await
    }

    /// Reset the USB transport of the first available device, without opening a
    /// session. See [`MtpDevice::reset_by_serial`] for the full contract.
    ///
    /// **Last resort on Android**: the reset can break the phone's MTP function
    /// until the user physically replugs. Try spaced reopens first.
    pub async fn reset_first(self) -> Result<(), Error> {
        let devices = NusbTransport::list_mtp_devices_with_known(&self.known_devices)?;
        let device_info = devices
            .into_iter()
            .next()
            .ok_or(crate::PtpError::NoDevice)?;
        self.reset_usb_device(device_info).await
    }

    /// Claim the interface and send the transport reset. Deliberately does NOT
    /// call [`PtpSession::open`] or `GetDeviceInfo`: claiming is all a wedged
    /// device can still answer.
    async fn reset_usb_device(
        self,
        device_info: crate::transport::UsbDeviceInfo,
    ) -> Result<(), Error> {
        let device = device_info.open().map_err(crate::PtpError::Usb)?;
        let transport = NusbTransport::open_with_timeout(device, self.timeout).await?;
        transport.reset_device().await?;
        Ok(())
    }

    /// Open a virtual device backed by local filesystem directories.
    ///
    /// This creates a virtual MTP device that speaks the full binary protocol but
    /// operates against local directories instead of USB. Use this for testing MTP
    /// client code without real hardware.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::path::PathBuf;
    /// use mtp_rs::MtpDevice;
    /// use mtp_rs::transport::virtual_device::config::{VirtualDeviceConfig, VirtualStorageConfig};
    ///
    /// # async fn example() -> Result<(), mtp_rs::Error> {
    /// let device = MtpDevice::builder()
    ///     .open_virtual(VirtualDeviceConfig {
    ///         manufacturer: "Google".into(),
    ///         model: "Virtual Pixel 9".into(),
    ///         serial: "virtual-001".into(),
    ///         storages: vec![VirtualStorageConfig {
    ///             description: "Internal Storage".into(),
    ///             capacity: 64 * 1024 * 1024 * 1024,
    ///             backing_dir: PathBuf::from("/tmp/mtp-test"),
    ///             read_only: false,
    ///         }],
    ///         ..Default::default()
    ///     })
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "virtual-device")]
    pub async fn open_virtual(
        self,
        config: crate::transport::virtual_device::config::VirtualDeviceConfig,
    ) -> Result<MtpDevice, Error> {
        if config.storages.is_empty() {
            return Err(Error::invalid_data(
                "VirtualDeviceConfig requires at least one storage",
            ));
        }

        let transport = crate::transport::virtual_device::VirtualTransport::new(config);
        let transport: Arc<dyn Transport> = Arc::new(transport);

        let session = Arc::new(self.open_ptp_session(transport.clone()).await?);

        // Get device info
        let device_info = session.get_device_info().await?;

        let backend = UsbBackend::new(session, device_info);
        Ok(MtpDevice {
            backend: Arc::new(backend),
        })
    }
}

impl Default for MtpDeviceBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_devices_returns_ok() {
        assert!(MtpDevice::list_devices().is_ok());
    }

    #[tokio::test]
    async fn resetting_an_absent_device_reports_no_device() {
        let err = MtpDevice::reset_by_serial("no-such-device-serial")
            .await
            .expect_err("no USB device has that serial");
        assert!(matches!(err, Error::NoDevice), "got {err:?}");
    }

    #[cfg(feature = "virtual-device")]
    #[tokio::test]
    async fn resetting_a_virtual_device_says_it_has_no_transport_to_reset() {
        let dir = tempfile::tempdir().unwrap();
        let serial = "reset-virtual-serial";
        let config = crate::VirtualDeviceConfig {
            serial: serial.into(),
            storages: vec![crate::VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024,
                backing_dir: dir.path().to_path_buf(),
                read_only: false,
            }],
            ..Default::default()
        };
        let info = crate::register_virtual_device(&config);

        // A virtual device is a filesystem, not a USB link: there's no transport
        // state to reset. Saying so beats a puzzling "no device found" in a
        // consumer's test suite, which is where this will actually be hit.
        let err = MtpDevice::reset_by_serial(serial)
            .await
            .expect_err("a virtual device has no USB transport");
        assert!(matches!(err, Error::Unsupported), "got {err:?}");

        crate::unregister_virtual_device(info.location_id);
    }

    #[test]
    fn builder_timeout() {
        // Default value
        let builder = MtpDeviceBuilder::new();
        assert_eq!(builder.timeout, NusbTransport::DEFAULT_TIMEOUT);

        // Custom value
        let custom = MtpDeviceBuilder::new().timeout(Duration::from_secs(45));
        assert_eq!(custom.timeout, Duration::from_secs(45));
    }

    #[cfg(feature = "virtual-device")]
    #[tokio::test]
    async fn builder_reuses_existing_session_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::VirtualDeviceConfig {
            model: "Session Reuse Test".into(),
            serial: "session-reuse-test".into(),
            storages: vec![crate::VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024,
                backing_dir: dir.path().to_path_buf(),
                read_only: false,
            }],
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        };
        let transport: Arc<dyn Transport> = Arc::new(
            crate::transport::virtual_device::VirtualTransport::new(config),
        );
        let session_id = 0xBAAA_AAAD;

        let first = MtpDeviceBuilder::new()
            .reuse_existing_session(session_id)
            .open_ptp_session(transport.clone())
            .await
            .unwrap();
        drop(first);

        let reused = MtpDeviceBuilder::new()
            .reuse_existing_session(session_id)
            .open_ptp_session(transport)
            .await
            .unwrap();

        assert_eq!(reused.session_id().0, session_id);
        assert_eq!(
            reused.get_device_info().await.unwrap().model,
            "Session Reuse Test"
        );
    }

    #[test]
    fn device_info_display() {
        let with_serial = MtpDeviceInfo {
            vendor_id: 0x04e8,
            product_id: 0x6860,
            manufacturer: Some("Samsung".to_string()),
            product: Some("Galaxy S24".to_string()),
            serial_number: Some("ABC123".to_string()),
            location_id: 0x00200000,
            speed: None,
            match_reason: crate::transport::MtpMatchReason::StandardClass,
        };
        let display = with_serial.display();
        assert!(display.contains("Samsung") && display.contains("Galaxy S24"));
        assert!(display.contains("ABC123") && display.contains("00200000"));

        // Without serial
        let no_serial = MtpDeviceInfo {
            serial_number: None,
            ..with_serial.clone()
        };
        assert!(!no_serial.display().contains("serial:"));

        // Unknown manufacturer
        let unknown = MtpDeviceInfo {
            manufacturer: None,
            product: None,
            ..with_serial
        };
        assert!(unknown.display().contains("Unknown"));
    }

    #[cfg(feature = "virtual-device")]
    #[tokio::test]
    async fn open_virtual_empty_storages_rejected() {
        use crate::transport::virtual_device::config::VirtualDeviceConfig;

        let config = VirtualDeviceConfig {
            serial: "empty-001".into(),
            // The point of this test: an empty `storages` must be rejected.
            storages: vec![],
            ..Default::default()
        };

        let result = MtpDevice::builder().open_virtual(config).await;
        match result {
            Err(err) => assert!(
                err.to_string().contains("at least one storage"),
                "unexpected error: {}",
                err
            ),
            Ok(_) => panic!("expected error for empty storages"),
        }
    }

    #[tokio::test]
    #[ignore] // Requires real MTP device
    async fn real_device_operations() {
        let device = MtpDevice::open_first().await.unwrap();
        println!("Connected to: {}", device.device_info().model);
        for storage in device.storages().await.unwrap() {
            println!("Storage: {}", storage.info().description);
        }
        device.close().await.unwrap();
    }
}
