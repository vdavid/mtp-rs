//! MtpDevice - the main entry point for MTP operations.

use crate::mtp::{DeviceEvent, Storage};
use crate::ptp::{DeviceInfo, ObjectHandle, PtpSession, StorageId};
use crate::transport::{NusbTransport, Transport};
use crate::Error;
use std::sync::Arc;
use std::time::Duration;

/// Internal shared state for MtpDevice.
pub(crate) struct MtpDeviceInner {
    pub(crate) session: Arc<PtpSession>,
    pub(crate) device_info: DeviceInfo,
}

impl MtpDeviceInner {
    /// Check if the device is an Android device.
    ///
    /// Detected by looking for "android.com" in the vendor extension descriptor.
    /// Android devices have known MTP quirks (e.g., ObjectHandle::ALL doesn't work
    /// for recursive listing).
    #[must_use]
    pub fn is_android(&self) -> bool {
        self.device_info
            .vendor_extension_desc
            .to_lowercase()
            .contains("android.com")
    }
}

/// An MTP device connection.
///
/// This is the main entry point for interacting with MTP devices.
/// Use `MtpDevice::open_first()` to connect to the first available device,
/// or `MtpDevice::builder()` for more control.
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
///              storage.info().free_space_bytes);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct MtpDevice {
    inner: Arc<MtpDeviceInner>,
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
        let mut result: Vec<MtpDeviceInfo> = devices
            .into_iter()
            .map(|d| MtpDeviceInfo {
                vendor_id: d.vendor_id,
                product_id: d.product_id,
                manufacturer: d.manufacturer,
                product: d.product,
                serial_number: d.serial_number,
                location_id: d.location_id,
            })
            .collect();

        #[cfg(feature = "virtual-device")]
        result.extend(crate::transport::virtual_device::registry::list_virtual_devices());

        Ok(result)
    }

    /// Get device information.
    #[must_use]
    pub fn device_info(&self) -> &DeviceInfo {
        &self.inner.device_info
    }

    /// Get a reference to the underlying [`PtpSession`].
    ///
    /// Provides direct access to low-level PTP operations
    /// ([`execute`](PtpSession::execute),
    /// [`execute_with_send`](PtpSession::execute_with_send),
    /// [`execute_with_receive`](PtpSession::execute_with_receive))
    /// and per-device knobs like
    /// [`set_split_header_data`](PtpSession::set_split_header_data).
    #[must_use]
    pub fn session(&self) -> &PtpSession {
        &self.inner.session
    }

    /// Check if the device supports renaming objects.
    ///
    /// This checks for support of the SetObjectPropValue operation (0x9804),
    /// which is required to rename files and folders via the ObjectFileName property.
    ///
    /// # Returns
    ///
    /// Returns true if the device advertises SetObjectPropValue support.
    #[must_use]
    pub fn supports_rename(&self) -> bool {
        self.inner.device_info.supports_rename()
    }

    /// Get all storages on the device.
    pub async fn storages(&self) -> Result<Vec<Storage>, Error> {
        let ids = self.inner.session.get_storage_ids().await?;
        let mut storages = Vec::with_capacity(ids.len());
        for id in ids {
            let info = self.inner.session.get_storage_info(id).await?;
            storages.push(Storage::new(self.inner.clone(), id, info));
        }
        Ok(storages)
    }

    /// Get a specific storage by ID.
    pub async fn storage(&self, id: StorageId) -> Result<Storage, Error> {
        let info = self.inner.session.get_storage_info(id).await?;
        Ok(Storage::new(self.inner.clone(), id, info))
    }

    /// Get object handles in a storage.
    ///
    /// # Arguments
    ///
    /// * `storage_id` - Storage to search, or `StorageId::ALL` for all storages
    /// * `parent` - Parent folder handle, or `None` for root level only,
    ///   or `Some(ObjectHandle::ALL)` for recursive listing
    pub async fn get_object_handles(
        &self,
        storage_id: StorageId,
        parent: Option<ObjectHandle>,
    ) -> Result<Vec<ObjectHandle>, Error> {
        self.inner
            .session
            .get_object_handles(storage_id, None, parent)
            .await
    }

    /// Receive the next event from the device.
    ///
    /// This method awaits **indefinitely** on the USB interrupt endpoint until an
    /// event arrives or the device disconnects. Always wrap this in
    /// `tokio::time::timeout` (or equivalent) so you can check for shutdown.
    ///
    /// # Concurrency
    ///
    /// Event reading uses the USB interrupt endpoint, which is independent from the
    /// bulk endpoints used by file operations (`list_objects`, `get_object`, etc.).
    /// It is safe to call `next_event()` concurrently with other `MtpDevice` methods.
    ///
    /// If you wrap `MtpDevice` in a shared lock (for example, `Arc<Mutex<MtpDevice>>`),
    /// do **not** hold that lock while awaiting `next_event()` — it will block all file
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
        match self.inner.session.poll_event().await? {
            Some(container) => Ok(DeviceEvent::from_container(&container)),
            None => Err(Error::Timeout),
        }
    }

    /// Close the connection (also happens on drop).
    pub async fn close(self) -> Result<(), Error> {
        // Try to close gracefully, but Arc might have multiple references
        if let Ok(inner) = Arc::try_unwrap(self.inner) {
            if let Ok(session) = Arc::try_unwrap(inner.session) {
                session.close().await?;
            }
        }
        Ok(())
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
#[derive(Debug, Clone)]
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
}

impl MtpDeviceInfo {
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
}

impl MtpDeviceBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            timeout: NusbTransport::DEFAULT_TIMEOUT,
            known_devices: Vec::new(),
        }
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
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mtp_rs::mtp::MtpDevice;
    ///
    /// # async fn example() -> Result<(), mtp_rs::Error> {
    /// let known = &[(0x045E, 0x0710)];
    ///
    /// // Enumerate with the extra VID/PID...
    /// let devices = MtpDevice::list_devices_with_known(known)?;
    /// let info = devices.into_iter().next().ok_or(mtp_rs::Error::NoDevice)?;
    ///
    /// // ...and open with the same list so the builder can find the device.
    /// let device = MtpDevice::builder()
    ///     .known_devices(known)
    ///     .open_by_location(info.location_id)
    ///     .await?;
    ///
    /// // Apply any per-device knobs after opening.
    /// device.session().set_split_header_data(true);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn known_devices(mut self, known: &[(u16, u16)]) -> Self {
        self.known_devices = known.to_vec();
        self
    }

    /// Open the first available device.
    pub async fn open_first(self) -> Result<MtpDevice, Error> {
        let devices = NusbTransport::list_mtp_devices_with_known(&self.known_devices)?;
        let device_info = devices.into_iter().next().ok_or(Error::NoDevice)?;
        let device = device_info.open().map_err(Error::Usb)?;
        self.open_nusb_device(device).await
    }

    /// Open a device at a specific USB location (port).
    ///
    /// Use `MtpDevice::list_devices()` to get available location IDs.
    /// Also checks the virtual device registry when the `virtual-device` feature is enabled.
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
            .ok_or(Error::NoDevice)?;
        let device = device_info.open().map_err(Error::Usb)?;
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

        let devices = NusbTransport::list_mtp_devices_with_known(&self.known_devices)?;
        let device_info = devices
            .into_iter()
            .find(|d| d.serial_number.as_deref() == Some(serial))
            .ok_or(Error::NoDevice)?;
        let device = device_info.open().map_err(Error::Usb)?;
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
    /// fallback to any interface with the MTP endpoint layout. Per-device knobs
    /// like [`PtpSession::set_split_header_data`] can be applied after opening
    /// via [`MtpDevice::session()`].
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mtp_rs::mtp::MtpDevice;
    /// use nusb::MaybeFuture;
    ///
    /// # async fn example() -> Result<(), mtp_rs::Error> {
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

        // Open session (use session ID 1)
        let session = Arc::new(PtpSession::open(transport.clone(), 1).await?);

        // Get device info
        let device_info = session.get_device_info().await?;

        // Quirk for Garmin devices
        if device_info.manufacturer == "Garmin" {
            session.set_split_header_data(true);
        }

        let inner = Arc::new(MtpDeviceInner {
            session,
            device_info,
        });

        Ok(MtpDevice { inner })
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
    /// use std::time::Duration;
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
    ///         supports_rename: true,
    ///         event_poll_interval: Duration::from_millis(50),
    ///         watch_backing_dirs: true,
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

        // Open session (use session ID 1)
        let session = Arc::new(PtpSession::open(transport.clone(), 1).await?);

        // Get device info
        let device_info = session.get_device_info().await?;

        let inner = Arc::new(MtpDeviceInner {
            session,
            device_info,
        });

        Ok(MtpDevice { inner })
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

    #[test]
    fn builder_timeout() {
        // Default value
        let builder = MtpDeviceBuilder::new();
        assert_eq!(builder.timeout, NusbTransport::DEFAULT_TIMEOUT);

        // Custom value
        let custom = MtpDeviceBuilder::new().timeout(Duration::from_secs(45));
        assert_eq!(custom.timeout, Duration::from_secs(45));
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
            manufacturer: "TestCorp".into(),
            model: "Empty".into(),
            serial: "empty-001".into(),
            storages: vec![],
            supports_rename: false,
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
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
