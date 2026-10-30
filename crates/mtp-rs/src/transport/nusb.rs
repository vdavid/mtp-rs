//! USB transport implementation using nusb.

use super::{BulkStream, Transport};
use async_trait::async_trait;
use futures::lock::Mutex;
use futures_timer::Delay;
use nusb::descriptors::{InterfaceDescriptor, TransferType};
use nusb::transfer::{
    Buffer, Bulk, ControlOut, ControlType, Direction, In, Interrupt, Out, Recipient, TransferError,
};
use nusb::MaybeFuture;
use std::time::Duration;

/// Run a nusb operation that is backed by a blocking syscall to completion.
///
/// nusb returns `open`, `claim_interface`, `set_configuration`, `clear_halt`,
/// and `list_devices` as `MaybeFuture`s wrapping a blocking syscall (the
/// underlying ioctl), *not* the async URB event loop. They must be `.wait()`-ed,
/// never `.await`-ed: awaiting one panics at runtime ("Awaiting blocking syscall
/// without an async runtime") unless the consumer enables nusb's `tokio`/`smol`
/// feature, which we deliberately don't (this crate is runtime-agnostic).
///
/// The trap is that the type-erased `impl MaybeFuture` also implements
/// `IntoFuture`, so `.await` compiles fine and only blows up at runtime, on real
/// hardware — a STALL never fires against the mock/virtual transport, so a stray
/// `.await` sails through CI and only surfaces on a device (issue #12, and
/// acknowledged upstream in kevinmehall/nusb#212). Routing every such call
/// through this one helper removes the per-call-site `.wait()`-vs-`.await`
/// choice: there's a single sanctioned spelling and a single place that holds
/// the rationale.
///
/// **Not** for `control_in`/`control_out`: those are genuinely async (nusb's own
/// URB event loop), need no runtime, and stay `.await`. Don't funnel them here.
fn blocking<F: MaybeFuture>(op: F) -> F::Output {
    op.wait()
}

/// MTP interface class code (Still Image).
const MTP_CLASS_IMAGE: u8 = 0x06;
/// MTP interface class code (Vendor-specific).
const MTP_CLASS_VENDOR: u8 = 0xFF;
/// MTP subclass code.
const MTP_SUBCLASS: u8 = 0x01;
/// MTP protocol code (PTP).
const MTP_PROTOCOL: u8 = 0x01;

// USB Still Image Class (SIC) class-specific request constants.
/// SIC Cancel Request bRequest code.
const SIC_CANCEL_REQUEST: u8 = 0x64;
/// CancelTransaction event code for the cancel payload.
const SIC_CANCEL_EVENT_CODE: u16 = 0x4001;
/// SIC Device Reset Request bRequest code. Returns the device to the Idle
/// state without re-enumerating, clearing a stuck transaction.
const SIC_DEVICE_RESET_REQUEST: u8 = 0x66;
/// SIC Get Device Status Request bRequest code.
const SIC_GET_DEVICE_STATUS_REQUEST: u8 = 0x67;
/// PTP response code Device_Busy, reported in a device status response while
/// the device is still processing a cancel.
const SIC_STATUS_DEVICE_BUSY: u16 = 0x2019;

/// Parse a SIC GET_DEVICE_STATUS response.
///
/// Layout: wLength (u16 LE, total meaningful bytes), Code (u16 LE, a PTP
/// response code), then optional 32-bit parameters. When the device has
/// stalled its bulk endpoint(s) during a cancel, the parameters carry the
/// stalled endpoint addresses.
///
/// Returns `(code, stalled_endpoint_addresses)`, or `None` when the response
/// is too short to contain a status code.
fn parse_device_status(data: &[u8]) -> Option<(u16, Vec<u8>)> {
    if data.len() < 4 {
        return None;
    }
    let wlength = u16::from_le_bytes([data[0], data[1]]) as usize;
    let code = u16::from_le_bytes([data[2], data[3]]);

    let end = data.len().min(wlength);
    let mut endpoints = Vec::new();
    let mut offset = 4;
    while offset + 4 <= end {
        let param = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        // The endpoint address lives in the low byte of the 32-bit parameter.
        endpoints.push(param as u8);
        offset += 4;
    }
    Some((code, endpoints))
}

/// Clear the endpoint's halt after a STALL so the pipe is usable again, then
/// convert the error.
///
/// Devices STALL a bulk endpoint to signal errors (PTP cameras do this for
/// unsupported operations and properties). The halt persists until the host
/// clears it (even across process restarts), so skipping this wedges every
/// subsequent transfer on the endpoint (observed on the Panasonic Lumix
/// DMC-TZ61, issue #12: a re-run of `ptp_diagnose` failed at `GetDeviceInfo`
/// with "endpoint stalled" from the previous run's property probing).
/// `kIOReturnNotResponding`: what macOS returns on the transfer in flight when the
/// cable is pulled.
#[cfg(target_os = "macos")]
const IO_RETURN_NOT_RESPONDING: u32 = 0xe000_02ed;
/// `kIOReturnNotOpen`: what macOS returns on the next transfer after a pulled cable.
#[cfg(target_os = "macos")]
const IO_RETURN_NOT_OPEN: u32 = 0xe000_02cd;

/// Whether a transfer failed because the device is gone.
///
/// nusb (0.2.8) maps only `kIOReturnNoDevice` to `TransferError::Disconnected` on
/// macOS. A real cable pull returns `kIOReturnNotResponding` on the in-flight transfer
/// and `kIOReturnNotOpen` on the next one, both as `Unknown`, so we fold those in
/// here (verified on Pixel 9 Pro XL, macOS 27.0.1, cable pulled mid-upload,
/// 2026-10-30, #53). A wedged-but-plugged device answering `NotResponding` would also
/// read as a disconnect; that's the lesser evil, since the caller's hotplug watcher
/// settles real presence. Every site that classifies a `TransferError` must ask this,
/// not match `TransferError::Disconnected` directly.
fn is_disconnect(err: TransferError) -> bool {
    match err {
        TransferError::Disconnected => true,
        #[cfg(target_os = "macos")]
        TransferError::Unknown(IO_RETURN_NOT_RESPONDING | IO_RETURN_NOT_OPEN) => true,
        _ => false,
    }
}

fn clear_halt_if_stalled<T, D>(ep: &mut nusb::Endpoint<T, D>, err: TransferError) -> crate::PtpError
where
    T: nusb::transfer::BulkOrInterrupt,
    D: nusb::transfer::EndpointDirection,
{
    if matches!(err, TransferError::Stall) {
        // The transfer has already completed (with a stall), so the endpoint is
        // idle, which is what `clear_halt` requires. `clear_halt` is a blocking
        // syscall, hence `blocking()` (see its doc for why `.await` would panic).
        let _ = blocking(ep.clear_halt());
    }
    NusbTransport::convert_transfer_error(err)
}

/// Reads the outcome of the CLASS_CANCEL request: `true` means the device didn't act on it.
///
/// A device that STALLs it (POCO X6 Pro on Linux, #43) keeps sending the object. Giving up here
/// left the rest of it in the pipe and wedged the phone until a replug, so the caller drains
/// anyway: the device finishes the transfer and closes it with a response, which leaves the
/// session in sync. That costs the rest of the transfer.
///
/// A timeout (`Cancelled`, a Galaxy A57 that had already gone silent, #43) takes the same path, so
/// the status poll after the drains gets to tell a busy device from a wedged one. Returning
/// `Timeout` here skipped that check, and the caller never learned to reopen.
fn cancel_was_not_acted_on(result: Result<(), TransferError>) -> Result<bool, crate::PtpError> {
    match result {
        Ok(()) => Ok(false),
        Err(TransferError::Stall | TransferError::Cancelled) => Ok(true),
        Err(e) => Err(NusbTransport::convert_transfer_error(e)),
    }
}

/// Negotiated USB link speed for a device.
///
/// This is the speed the device, host port, and cable agreed on at enumeration,
/// not a static device capability. A USB 3.2 Gen 2 phone connected via a USB 2.0
/// charging cable reports `High` (480 Mbit/s).
///
/// Mirrors `nusb::Speed` so callers don't need a direct `nusb` dependency.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum UsbSpeed {
    /// USB 1.0 low-speed: 1.5 Mbit/s.
    Low,
    /// USB 1.1 full-speed: 12 Mbit/s.
    Full,
    /// USB 2.0 high-speed: 480 Mbit/s.
    High,
    /// USB 3.2 Gen 1 (formerly USB 3.0): 5 Gbit/s.
    Super,
    /// USB 3.2 Gen 2 (formerly USB 3.1 Gen 2): 10 Gbit/s.
    SuperPlus,
}

impl UsbSpeed {
    fn from_nusb(s: nusb::Speed) -> Option<Self> {
        match s {
            nusb::Speed::Low => Some(UsbSpeed::Low),
            nusb::Speed::Full => Some(UsbSpeed::Full),
            nusb::Speed::High => Some(UsbSpeed::High),
            nusb::Speed::Super => Some(UsbSpeed::Super),
            nusb::Speed::SuperPlus => Some(UsbSpeed::SuperPlus),
            // `nusb::Speed` is `#[non_exhaustive]`; unknown future variants → no info.
            _ => None,
        }
    }
}

/// Why a USB device was classified as an MTP candidate.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum MtpMatchReason {
    /// Device or interface class/subclass/protocol matched Still Image / MTP.
    StandardClass,
    /// The OS-published interface string identifies the interface as MTP.
    InterfaceString,
    /// Caller supplied the VID/PID through `known` devices.
    KnownVidPid,
    /// The opened configuration descriptor has the MTP endpoint layout.
    OpenedDescriptorScan,
}

impl MtpMatchReason {
    /// Stable snake_case identifier for machine-readable output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StandardClass => "standard_class",
            Self::InterfaceString => "interface_string",
            Self::KnownVidPid => "known_vid_pid",
            Self::OpenedDescriptorScan => "opened_descriptor_scan",
        }
    }
}

/// USB device information with topology-based location ID.
///
/// Marked `#[non_exhaustive]` so future field additions don't break consumers
/// that pattern-match or destructure. Construct via the crate (return type of
/// [`NusbTransport::list_mtp_devices`]); consumers shouldn't build it directly.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct UsbDeviceInfo {
    /// USB vendor ID
    pub vendor_id: u16,
    /// USB product ID
    pub product_id: u16,
    /// Manufacturer name (e.g., "Google", "Samsung")
    pub manufacturer: Option<String>,
    /// Product name (e.g., "Pixel 9 Pro XL")
    pub product: Option<String>,
    /// Device serial number (if available)
    pub serial_number: Option<String>,
    /// USB location identifier derived from bus and port topology (stable per port)
    pub location_id: u64,
    /// Negotiated USB link speed (slowest of host port, cable, device).
    /// `None` if the OS doesn't report it for this device.
    pub speed: Option<UsbSpeed>,
    /// Why this device was classified as an MTP candidate.
    pub match_reason: MtpMatchReason,
    /// Reference to the underlying nusb device info for opening
    nusb_info: nusb::DeviceInfo,
}

impl UsbDeviceInfo {
    /// Open the USB device.
    pub fn open(&self) -> Result<nusb::Device, nusb::Error> {
        blocking(self.nusb_info.open())
    }
}

/// USB transport implementation using nusb.
pub struct NusbTransport {
    interface: nusb::Interface,
    interface_number: u8,
    bulk_in: Mutex<nusb::Endpoint<Bulk, In>>,
    bulk_out: Mutex<nusb::Endpoint<Bulk, Out>>,
    interrupt_in: Mutex<nusb::Endpoint<Interrupt, In>>,
    /// Timeout for bulk transfers (sending commands, receiving data).
    timeout: Duration,
}

impl NusbTransport {
    /// Default timeout for bulk transfers (30 seconds for large file transfers).
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

    /// Default buffer size for interrupt transfers.
    const INTERRUPT_BUFFER_SIZE: usize = 64;

    /// Read size for draining a transfer the device refused to cancel (see `cancel_transfer`).
    const REFUSED_CANCEL_DRAIN_READ: usize = 64 * 1024;

    /// List all available MTP devices with location IDs.
    pub fn list_mtp_devices() -> Result<Vec<UsbDeviceInfo>, crate::PtpError> {
        Self::list_mtp_devices_with_known(&[])
    }

    /// List all available MTP devices, including additional devices identified
    /// by the given VID/PID pairs.
    ///
    /// Devices matching the provided VID/PID pairs are included in the results
    /// even if their USB descriptors don't match standard MTP class codes.
    pub fn list_mtp_devices_with_known(
        known: &[(u16, u16)],
    ) -> Result<Vec<UsbDeviceInfo>, crate::PtpError> {
        let devices = blocking(nusb::list_devices())
            .map_err(crate::PtpError::Usb)?
            .filter_map(|dev| {
                let match_reason = Self::mtp_match_reason(&dev, known)?;
                let location_id = location_id_from_topology(&dev);
                let speed = dev.speed().and_then(UsbSpeed::from_nusb);
                Some(UsbDeviceInfo {
                    vendor_id: dev.vendor_id(),
                    product_id: dev.product_id(),
                    manufacturer: dev.manufacturer_string().map(String::from),
                    product: dev.product_string().map(String::from),
                    serial_number: dev.serial_number().map(String::from),
                    location_id,
                    speed,
                    match_reason,
                    nusb_info: dev,
                })
            })
            .collect();
        Ok(devices)
    }

    /// Check if a device info represents an MTP device.
    ///
    /// A device is considered MTP if it matches standard MTP class codes, has
    /// an interface with the MTP endpoint layout, or matches one of the
    /// caller-provided VID/PID pairs (used for devices with non-standard USB
    /// descriptors that still speak MTP).
    fn mtp_match_reason(dev: &nusb::DeviceInfo, known: &[(u16, u16)]) -> Option<MtpMatchReason> {
        // Fast path: caller-supplied known devices that may use non-standard descriptors.
        if known
            .iter()
            .any(|&(v, p)| v == dev.vendor_id() && p == dev.product_id())
        {
            return Some(MtpMatchReason::KnownVidPid);
        }

        // Check device class/subclass/protocol at device level.
        if Self::is_mtp_class(dev.class(), dev.subclass(), dev.protocol()) {
            return Some(MtpMatchReason::StandardClass);
        }

        // Many devices are composite (class 0) or vendor-specific (class 0xFF)
        // with MTP on one interface. Only inspect these further.
        if dev.class() != 0 && dev.class() != MTP_CLASS_VENDOR {
            return None;
        }

        // Check interface-level class info available from DeviceInfo without opening.
        for intf in dev.interfaces() {
            if let Some(reason) = Self::mtp_summary_match_reason(
                intf.class(),
                intf.subclass(),
                intf.protocol(),
                intf.interface_string(),
            ) {
                return Some(reason);
            }
        }

        // Fall back to opening the device and inspecting full configuration descriptors.
        // This also catches vendor-specific interfaces (class 0xFF) that use non-standard
        // subclass/protocol but have the MTP endpoint layout (e.g. Amazon Kindle).
        if let Ok(device) = blocking(dev.open()) {
            if let Ok(config) = device.active_configuration() {
                for interface in config.interfaces() {
                    if let Some(alt) = interface.alt_settings().next() {
                        if Self::is_mtp_interface(&alt) {
                            return Some(MtpMatchReason::OpenedDescriptorScan);
                        }
                    }
                }
            }
        }

        None
    }

    fn mtp_summary_match_reason(
        class: u8,
        subclass: u8,
        protocol: u8,
        interface_string: Option<&str>,
    ) -> Option<MtpMatchReason> {
        if Self::is_mtp_class(class, subclass, protocol) {
            return Some(MtpMatchReason::StandardClass);
        }
        if interface_string.is_some_and(|s| s.trim().eq_ignore_ascii_case("MTP")) {
            return Some(MtpMatchReason::InterfaceString);
        }
        None
    }

    /// Check if class/subclass/protocol match standard MTP identifiers.
    fn is_mtp_class(class: u8, subclass: u8, protocol: u8) -> bool {
        (class == MTP_CLASS_IMAGE || class == MTP_CLASS_VENDOR)
            && subclass == MTP_SUBCLASS
            && protocol == MTP_PROTOCOL
    }

    /// Check if an interface descriptor looks like an MTP interface.
    ///
    /// Matches standard MTP class/subclass/protocol, and also vendor-specific
    /// interfaces (class 0xFF) with non-standard subclass/protocol that have
    /// the MTP endpoint layout (bulk IN + bulk OUT + interrupt IN). Some devices
    /// like Amazon Kindle use vendor-specific descriptors while still speaking MTP.
    fn is_mtp_interface(alt: &InterfaceDescriptor) -> bool {
        if Self::is_mtp_class(alt.class(), alt.subclass(), alt.protocol()) {
            return true;
        }
        // For vendor-specific class, subclass and protocol are vendor-defined,
        // so we can't rely on them. Use endpoint layout as a heuristic instead.
        alt.class() == MTP_CLASS_VENDOR && Self::has_mtp_endpoint_layout(alt)
    }

    /// Check if an interface has the MTP endpoint layout:
    /// one bulk IN, one bulk OUT, and one interrupt IN endpoint.
    fn has_mtp_endpoint_layout(alt: &InterfaceDescriptor) -> bool {
        let mut bulk_in = false;
        let mut bulk_out = false;
        let mut interrupt_in = false;
        for ep in alt.endpoints() {
            match (ep.direction(), ep.transfer_type()) {
                (Direction::In, TransferType::Bulk) => bulk_in = true,
                (Direction::Out, TransferType::Bulk) => bulk_out = true,
                (Direction::In, TransferType::Interrupt) => interrupt_in = true,
                _ => {}
            }
        }
        bulk_in && bulk_out && interrupt_in
    }

    /// Whether a `claim_interface` failure looks like the OS hasn't published
    /// the interface yet (rather than a permanent error).
    ///
    /// On macOS, vendor-class or class-0 devices that IOKit doesn't
    /// auto-configure end up with no `IOUSBHostInterface` services published,
    /// even when the device's configuration descriptor reports otherwise.
    /// The resulting `claim_interface` error is `NotFound` (there's nothing
    /// for nusb to claim), and the fix is to issue `SetConfiguration(1)`,
    /// which makes IOKit publish the interface objects.
    #[cfg(target_os = "macos")]
    fn is_interface_unpublished(e: &nusb::Error) -> bool {
        matches!(e.kind(), nusb::ErrorKind::NotFound)
    }

    /// Open a specific device and claim the MTP interface.
    pub async fn open(device: nusb::Device) -> Result<Self, crate::PtpError> {
        Self::open_with_timeout(device, Self::DEFAULT_TIMEOUT).await
    }

    /// Open with custom bulk transfer timeout.
    ///
    /// The interface scan first looks for a strict MTP-class interface; if none
    /// is found, it falls back to any interface with the MTP endpoint layout
    /// (bulk IN + bulk OUT + interrupt IN). This relaxed fallback supports
    /// legacy devices that report a non-standard interface class. The caller
    /// has already hand-picked the device, so the scan can be permissive at
    /// this point.
    pub async fn open_with_timeout(
        device: nusb::Device,
        timeout: Duration,
    ) -> Result<Self, crate::PtpError> {
        // Find the MTP interface
        let config = device.active_configuration().map_err(|e| {
            crate::PtpError::invalid_data(format!("Failed to get configuration: {}", e))
        })?;

        let mut mtp_interface_number = None;
        let mut bulk_in_addr = None;
        let mut bulk_out_addr = None;
        let mut interrupt_in_addr = None;

        // Two-pass scan: prefer a strictly-matching MTP interface, but fall
        // back to any interface with the MTP endpoint layout. The caller has
        // already hand-picked the device, so a permissive fallback is safe and
        // supports legacy devices that report a non-standard interface class.
        let pick = |strict: bool| {
            for interface in config.interfaces() {
                let Some(alt_setting) = interface.alt_settings().next() else {
                    continue;
                };
                let matches = if strict {
                    Self::is_mtp_interface(&alt_setting)
                } else {
                    Self::has_mtp_endpoint_layout(&alt_setting)
                };
                if matches {
                    let mut bin = None;
                    let mut bout = None;
                    let mut iin = None;
                    for endpoint in alt_setting.endpoints() {
                        match (endpoint.direction(), endpoint.transfer_type()) {
                            (Direction::Out, TransferType::Bulk) => bout = Some(endpoint.address()),
                            (Direction::In, TransferType::Bulk) => bin = Some(endpoint.address()),
                            (Direction::In, TransferType::Interrupt) => {
                                iin = Some(endpoint.address())
                            }
                            _ => {}
                        }
                    }
                    return Some((interface.interface_number(), bin, bout, iin));
                }
            }
            None
        };

        if let Some((n, bin, bout, iin)) = pick(true).or_else(|| pick(false)) {
            mtp_interface_number = Some(n);
            bulk_in_addr = bin;
            bulk_out_addr = bout;
            interrupt_in_addr = iin;
        }

        let interface_number = mtp_interface_number
            .ok_or_else(|| crate::PtpError::invalid_data("No MTP interface found on device"))?;

        let bulk_in_addr = bulk_in_addr
            .ok_or_else(|| crate::PtpError::invalid_data("No bulk IN endpoint found"))?;
        let bulk_out_addr = bulk_out_addr
            .ok_or_else(|| crate::PtpError::invalid_data("No bulk OUT endpoint found"))?;
        let interrupt_in_addr = interrupt_in_addr
            .ok_or_else(|| crate::PtpError::invalid_data("No interrupt IN endpoint found"))?;

        // Claim the interface
        //
        // macOS: IOKit doesn't publish interface services for vendor-class /
        // class-0 devices with no matching driver. Force-set configuration 1
        // so IOKit publishes them, then retry.
        let interface = match blocking(device.claim_interface(interface_number)) {
            Ok(iface) => iface,
            #[cfg(target_os = "macos")]
            Err(e) if Self::is_interface_unpublished(&e) => {
                blocking(device.set_configuration(1)).map_err(crate::PtpError::Usb)?;
                blocking(device.claim_interface(interface_number)).map_err(crate::PtpError::Usb)?
            }
            Err(e) => return Err(crate::PtpError::Usb(e)),
        };

        // Open endpoints
        let bulk_in = interface
            .endpoint::<Bulk, In>(bulk_in_addr)
            .map_err(crate::PtpError::Usb)?;
        let bulk_out = interface
            .endpoint::<Bulk, Out>(bulk_out_addr)
            .map_err(crate::PtpError::Usb)?;
        let interrupt_in = interface
            .endpoint::<Interrupt, In>(interrupt_in_addr)
            .map_err(crate::PtpError::Usb)?;

        Ok(Self {
            interface,
            interface_number,
            bulk_in: Mutex::new(bulk_in),
            bulk_out: Mutex::new(bulk_out),
            interrupt_in: Mutex::new(interrupt_in),
            timeout,
        })
    }

    /// Get the bulk transfer timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Set the bulk transfer timeout.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Convert a nusb TransferError to crate::PtpError.
    fn convert_transfer_error(err: TransferError) -> crate::PtpError {
        if is_disconnect(err) {
            return crate::PtpError::Disconnected;
        }
        match err {
            // send_bulk uses transfer_blocking, which cancels the transfer on
            // timeout and returns Cancelled. Map to Timeout so that
            // Error::is_retryable() treats it correctly.
            TransferError::Cancelled => crate::PtpError::Timeout,
            TransferError::Disconnected => crate::PtpError::Disconnected,
            TransferError::Stall
            | TransferError::Fault
            | TransferError::InvalidArgument
            | TransferError::Unknown(_) => {
                crate::PtpError::Io(std::io::Error::other(err.to_string()))
            }
        }
    }

    /// Poll SIC GET_DEVICE_STATUS until the device reports it has finished
    /// processing a cancel, clearing any bulk endpoint halts it reports.
    ///
    /// SIC-compliant devices (PTP cameras) report Device_Busy (0x2019) while
    /// a cancel is in progress and expect the host to poll until the status
    /// turns OK. Skipping this leaves them permanently unresponsive: on the
    /// Panasonic Lumix DMC-TZ61 (issue #12), every operation after a
    /// "successful" cancel timed out until a battery pull.
    ///
    /// This must run AFTER the bulk/interrupt drains (see `cancel_transfer`):
    /// inserting it between CLASS_CANCEL and the drain breaks Android.
    /// Android doesn't implement this control request at all, so a `Stall`
    /// (unsupported) is treated as "nothing to wait for" and ignored.
    ///
    /// Returns `true` if the device appears **wedged**: it stopped answering the
    /// status request mid-poll, surfaced as a control-transfer timeout
    /// (`TransferError::Cancelled`). That is distinct from the device *declining*
    /// the request (`Stall`, i.e. unsupported) or answering `OK`. A wedged result
    /// tells [`cancel_transfer`](Self::cancel_transfer) to escalate to a device
    /// reset (Samsung cancel wedge, issue #18).
    async fn settle_after_cancel(&self) -> bool {
        const POLL_INTERVAL: Duration = Duration::from_millis(50);
        // 100 polls x 50ms = 5s cap, far beyond any sane cancel processing.
        const MAX_POLLS: u32 = 100;

        for _ in 0..MAX_POLLS {
            let result = self
                .interface
                .control_in(
                    nusb::transfer::ControlIn {
                        control_type: ControlType::Class,
                        recipient: Recipient::Interface,
                        request: SIC_GET_DEVICE_STATUS_REQUEST,
                        value: 0,
                        index: self.interface_number as u16,
                        length: 64,
                    },
                    Duration::from_millis(300),
                )
                .await;

            let data = match result {
                Ok(data) => data,
                // A control timeout (Cancelled) means the device has gone
                // silent on endpoint 0 too: the cancel wedged it (Samsung, #18).
                // Any other error is the device refusing the request (a Stall
                // for "unsupported", as most Android devices do), which is
                // normal and not a wedge.
                Err(TransferError::Cancelled) => {
                    diag_debug!(
                        "settle_after_cancel: GET_DEVICE_STATUS timed out (Cancelled) -> WEDGED"
                    );
                    return true;
                }
                Err(e) => {
                    diag_trace!(
                        "settle_after_cancel: GET_DEVICE_STATUS refused ({:?}) -> not wedged",
                        e
                    );
                    return false;
                }
            };
            let Some((code, stalled_endpoints)) = parse_device_status(&data) else {
                diag_trace!(
                    "settle_after_cancel: unparseable status ({} bytes)",
                    data.len()
                );
                return false; // Unparseable response: don't insist.
            };
            diag_trace!(
                "settle_after_cancel: status code={:#06x}, stalled_endpoints={:?}",
                code,
                stalled_endpoints
            );
            for address in stalled_endpoints {
                self.clear_bulk_halt_by_address(address).await;
            }
            if code != SIC_STATUS_DEVICE_BUSY {
                return false;
            }
            Delay::new(POLL_INTERVAL).await;
        }
        diag_debug!(
            "settle_after_cancel: still Device_Busy after {} polls",
            MAX_POLLS
        );
        false
    }

    /// Clear the halt condition on the bulk endpoint with the given address,
    /// consuming any pending transfers first (nusb requires the endpoint to
    /// be idle when clearing). Unknown addresses are ignored.
    async fn clear_bulk_halt_by_address(&self, address: u8) {
        {
            let mut ep = self.bulk_in.lock().await;
            if ep.endpoint_address() == address {
                if ep.pending() > 0 {
                    ep.cancel_all();
                    while ep.pending() > 0 {
                        let _ = ep.next_complete().await;
                    }
                }
                let _ = blocking(ep.clear_halt());
                return;
            }
        }
        let mut ep = self.bulk_out.lock().await;
        if ep.endpoint_address() == address {
            if ep.pending() > 0 {
                ep.cancel_all();
                while ep.pending() > 0 {
                    let _ = ep.next_complete().await;
                }
            }
            let _ = blocking(ep.clear_halt());
        }
    }
}

#[async_trait]
impl Transport for NusbTransport {
    async fn send_bulk(&self, data: &[u8]) -> Result<(), crate::PtpError> {
        let mut ep = self.bulk_out.lock().await;
        let buf: Buffer = data.to_vec().into();
        let completion = ep.transfer_blocking(buf, self.timeout);
        if let Err(e) = completion.status {
            return Err(clear_halt_if_stalled(&mut ep, e));
        }
        Ok(())
    }

    async fn send_bulk_streaming(&self, chunks: BulkStream<'_>) -> Result<(), crate::PtpError> {
        use futures::StreamExt;

        let mut ep = self.bulk_out.lock().await;
        let max_packet_size = ep.max_packet_size();
        let transfer_size: usize = 256 * 1024; // 256KB per USB transfer
                                               // Round up to max_packet_size boundary.
        let transfer_size = (transfer_size.div_ceil(max_packet_size)).max(1) * max_packet_size;

        let mut current_buf = ep.allocate(transfer_size);
        let mut total_sent = 0usize;
        let mut stream = chunks;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(crate::PtpError::Io)?;
            let mut remaining = chunk.as_ref();

            while !remaining.is_empty() {
                let space = current_buf.remaining_capacity();
                let to_copy = remaining.len().min(space);
                current_buf.extend_from_slice(&remaining[..to_copy]);
                remaining = &remaining[to_copy..];

                if current_buf.remaining_capacity() == 0 {
                    ep.submit(current_buf);
                    let completion = ep
                        .wait_next_complete(self.timeout)
                        .ok_or(crate::PtpError::Timeout)?;
                    if let Err(e) = completion.status {
                        return Err(clear_halt_if_stalled(&mut ep, e));
                    }
                    total_sent += transfer_size;
                    current_buf = ep.allocate(transfer_size);
                }
            }
        }

        // Send remaining data.
        let final_len = current_buf.len();
        if final_len > 0 {
            ep.submit(current_buf);
            let completion = ep
                .wait_next_complete(self.timeout)
                .ok_or(crate::PtpError::Timeout)?;
            if let Err(e) = completion.status {
                return Err(clear_halt_if_stalled(&mut ep, e));
            }

            // If the final transfer was a multiple of max_packet_size, send ZLP
            // so the device sees a short packet delimiter.
            if final_len % max_packet_size == 0 {
                ep.submit(Buffer::new(0));
                let completion = ep
                    .wait_next_complete(self.timeout)
                    .ok_or(crate::PtpError::Timeout)?;
                if let Err(e) = completion.status {
                    return Err(clear_halt_if_stalled(&mut ep, e));
                }
            }
        } else if total_sent > 0 && total_sent.is_multiple_of(max_packet_size) {
            // All data fit in full transfers. Send ZLP to terminate.
            ep.submit(Buffer::new(0));
            let completion = ep
                .wait_next_complete(self.timeout)
                .ok_or(crate::PtpError::Timeout)?;
            if let Err(e) = completion.status {
                return Err(clear_halt_if_stalled(&mut ep, e));
            }
        }

        Ok(())
    }

    async fn receive_bulk(&self, max_size: usize) -> Result<Vec<u8>, crate::PtpError> {
        let mut ep = self.bulk_in.lock().await;

        // If there's no pending transfer from a previous timed-out call,
        // submit a new one. Otherwise, the pending transfer already has our
        // data in flight and we just need to wait for it.
        if ep.pending() == 0 {
            let max_packet_size = ep.max_packet_size();
            let aligned_size = align_to_packet_size(max_size, max_packet_size);
            ep.submit(Buffer::new(aligned_size));
        }

        // Wait for the transfer to complete OR the timeout to expire.
        // next_complete() is cancel-safe: dropping its future does NOT cancel
        // the underlying USB transfer. On timeout we leave the transfer pending
        // so a subsequent call picks up the in-flight data.
        //
        // The select result is unwrapped inside a block so the Either (which
        // borrows `ep` through the next_complete future) is dropped before we
        // need `ep` again for halt clearing.
        let completed = {
            let selected = futures::future::select(
                Box::pin(ep.next_complete()),
                Box::pin(Delay::new(self.timeout)),
            )
            .await;
            match selected {
                futures::future::Either::Left((comp, _)) => Some(comp),
                futures::future::Either::Right((_, _)) => None,
            }
        };

        match completed {
            Some(comp) => {
                if let Err(e) = comp.status {
                    return Err(clear_halt_if_stalled(&mut ep, e));
                }
                Ok(comp.buffer[..comp.actual_len].to_vec())
            }
            None => {
                // Don't cancel the transfer; it stays pending in the endpoint.
                // next_complete() is cancel-safe, so dropping its future is fine.
                // On retry, the next call will find pending() > 0 and pick it up.
                Err(crate::PtpError::Timeout)
            }
        }
    }

    async fn receive_interrupt(&self) -> Result<Vec<u8>, crate::PtpError> {
        let mut ep = self.interrupt_in.lock().await;

        // Submit a new transfer only if none is already pending.
        if ep.pending() == 0 {
            let max_packet_size = ep.max_packet_size();
            let aligned_size = align_to_packet_size(Self::INTERRUPT_BUFFER_SIZE, max_packet_size);
            ep.submit(Buffer::new(aligned_size));
        }

        // Await indefinitely; callers handle cancellation via async
        // cancellation (e.g. tokio::time::timeout or select!).
        let completion = ep.next_complete().await;
        if let Err(e) = completion.status {
            return Err(clear_halt_if_stalled(&mut ep, e));
        }
        Ok(completion.buffer[..completion.actual_len].to_vec())
    }

    /// USB Still Image Class cancel implementation.
    ///
    /// Getting mid-transfer cancellation right on MTP/PTP devices is notoriously
    /// tricky. Different devices react differently to cancel signals, and getting
    /// the timing wrong can leave the device unresponsive until physical replug.
    ///
    /// This implementation follows the approach proven by libmtp's
    /// `ptp_read_cancel_func` (contributed by Florent Viard in 2017, PR #2),
    /// which was developed through extensive real-device testing after discovering
    /// that naive approaches caused Nexus 6P to fail all subsequent operations
    /// and GoPro Hero 5 to freeze completely.
    ///
    /// The key insight: after sending the CLASS_CANCEL control request, the drain
    /// must start *immediately*. The device stops sending new data quickly, but
    /// data already in the USB pipeline must be read and discarded before the
    /// pipe goes idle. Inserting any delay (like polling GET_DEVICE_STATUS, which
    /// Android doesn't support anyway) causes the drain to find an empty pipe
    /// while the device's MTP state machine remains stuck.
    ///
    /// AFTER the drains, though, GET_DEVICE_STATUS polling is required: the SIC
    /// spec says the device reports Device_Busy while the cancel is processed,
    /// and SIC-compliant devices (PTP cameras) wait for the host to poll until
    /// the status turns OK before accepting new operations. Without this step,
    /// the Panasonic Lumix DMC-TZ61 (issue #12) timed out on every operation
    /// after a "successful" cancel until a battery pull. Android doesn't
    /// implement the request; its failure is ignored.
    ///
    /// The sequence is:
    /// 1. Send CLASS_CANCEL control request (bRequest=0x64, 300ms timeout). A device
    ///    that STALLs it keeps sending, so the drain then reads the rest of the object
    /// 2. Drain bulk IN pipe (read + discard until the response or idle_timeout,
    ///    maxpacket chunks, 64 KiB ones after a refused cancel)
    /// 3. Drain interrupt pipe (consume CancelTransaction event if present)
    /// 4. Poll GET_DEVICE_STATUS (bRequest=0x67) until not Device_Busy,
    ///    clearing any bulk endpoint halts the device reports
    ///
    /// References:
    /// - libmtp PR #2: <https://github.com/libmtp/libmtp/pull/2>
    /// - USB Still Image Capture Device Definition, Section 5
    async fn cancel_transfer(
        &self,
        transaction_id: u32,
        idle_timeout: Duration,
    ) -> Result<(), crate::PtpError> {
        // Step 1: Send CLASS_CANCEL control request (bRequest=0x64).
        //
        // 300ms timeout matches libmtp and Windows behavior. This is a
        // class-specific control transfer on endpoint 0, independent of
        // the bulk pipes. The 6-byte payload contains the CancelTransaction
        // event code (0x4001) and the transaction ID to cancel.
        diag_debug!(
            "cancel_transfer: begin (txn={}, idle_timeout={:?})",
            transaction_id,
            idle_timeout
        );

        let mut payload = [0u8; 6];
        payload[0..2].copy_from_slice(&SIC_CANCEL_EVENT_CODE.to_le_bytes());
        payload[2..6].copy_from_slice(&transaction_id.to_le_bytes());

        let cancel_result = self
            .interface
            .control_out(
                ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: SIC_CANCEL_REQUEST,
                    value: 0,
                    index: self.interface_number as u16,
                    data: &payload,
                },
                Duration::from_millis(300),
            )
            .await;
        let refused = cancel_was_not_acted_on(cancel_result.map(|_| ()))?;
        diag_debug!(
            "cancel_transfer: step 1 CLASS_CANCEL sent (refused={})",
            refused
        );

        // Step 2: Drain bulk IN pipe.
        //
        // Read and discard data in maxpacket-sized chunks until idle_timeout
        // fires (no more data arriving). The device may still be sending data
        // that was already in the USB pipeline when CLASS_CANCEL arrived. The
        // drain also catches the Response container (Transaction_Cancelled or
        // Ok) that the device sends to end the transaction.
        //
        // IMPORTANT: This must happen immediately after the control request.
        // Any delay (like polling GET_DEVICE_STATUS) allows the device to
        // give up waiting for us to read, leaving its MTP state machine stuck.
        {
            let mut ep = self.bulk_in.lock().await;
            let max_packet_size = ep.max_packet_size();
            // Maxpacket reads are the validated libmtp shape for a real cancel. After a
            // refused one the whole remaining object comes through, so read big.
            let read_size = if refused {
                align_to_packet_size(Self::REFUSED_CANCEL_DRAIN_READ, max_packet_size)
            } else {
                align_to_packet_size(max_packet_size, max_packet_size)
            };
            // Diagnostic counters (surfaced via the `tracing` feature). A #18
            // wedge shows up here as a drain that ends with no closing
            // Response container (at any size: 36 bytes was enough on an S23). Kept unconditionally; the cost is negligible.
            let mut drained_packets = 0u32;
            let mut drained_bytes = 0usize;
            let mut saw_response = false;
            loop {
                if ep.pending() == 0 {
                    ep.submit(Buffer::new(read_size));
                }

                let drain_result = {
                    let complete_fut = ep.next_complete();
                    let timeout_fut = Delay::new(idle_timeout);
                    futures::pin_mut!(complete_fut, timeout_fut);

                    match futures::future::select(complete_fut, timeout_fut).await {
                        futures::future::Either::Left((completion, _)) => {
                            match completion.status {
                                Ok(()) => {
                                    drained_packets += 1;
                                    drained_bytes += completion.actual_len;
                                    let read = &completion.buffer[..completion.actual_len];
                                    if is_response_container(read, read_size) {
                                        saw_response = true;
                                        Ok(true) // Response received, done
                                    } else {
                                        Ok(false) // Data, keep draining
                                    }
                                }
                                Err(e) if is_disconnect(e) => Err(crate::PtpError::Disconnected),
                                Err(TransferError::Cancelled) => Ok(true),
                                Err(_) => Ok(true),
                            }
                        }
                        futures::future::Either::Right((_, _)) => {
                            // Idle timeout: no more data arriving, pipe is clear.
                            Ok(true)
                        }
                    }
                };

                match drain_result {
                    Ok(true) => {
                        ep.cancel_all();
                        while ep.pending() > 0 {
                            let _ = ep.next_complete().await;
                        }
                        break;
                    }
                    Ok(false) => continue,
                    Err(e) => return Err(e),
                }
            }
            diag_debug!(
                "cancel_transfer: step 2 bulk drain done ({} data packets, {} bytes, saw_response={})",
                drained_packets,
                drained_bytes,
                saw_response
            );
        }

        // Step 3: Drain interrupt pipe.
        //
        // Consume the CancelTransaction event if the device sent one. This is
        // critical for some devices: GoPro Hero 5 stops responding entirely
        // if this event is left unread on the interrupt pipe.
        {
            let mut ep = self.interrupt_in.lock().await;
            if ep.pending() == 0 {
                let max_packet_size = ep.max_packet_size();
                let aligned_size =
                    align_to_packet_size(Self::INTERRUPT_BUFFER_SIZE, max_packet_size);
                ep.submit(Buffer::new(aligned_size));
            }

            let timed_out = {
                let complete_fut = ep.next_complete();
                let timeout_fut = Delay::new(idle_timeout);
                futures::pin_mut!(complete_fut, timeout_fut);

                matches!(
                    futures::future::select(complete_fut, timeout_fut).await,
                    futures::future::Either::Right(_)
                )
            };

            if timed_out {
                ep.cancel_all();
                while ep.pending() > 0 {
                    let _ = ep.next_complete().await;
                }
            }
            diag_debug!(
                "cancel_transfer: step 3 interrupt drain done (event_received={})",
                !timed_out
            );
        }

        // Step 4: Poll GET_DEVICE_STATUS until the device reports it's done
        // cancelling, clearing any endpoint halts it reports. Cameras need
        // this to accept new operations after a cancel; Android ignores it.
        let wedged = self.settle_after_cancel().await;
        diag_debug!("cancel_transfer: step 4 settle done (wedged={})", wedged);

        // Step 5: If the device went silent (Samsung cancel wedge, #18),
        // the drains couldn't realign the session and it's dead. A CLASS_CANCEL
        // can't revive it, but a session-less USB DEVICE_RESET un-sticks the
        // transport so the device answers again (verified on a Galaxy S23 Ultra),
        // no physical replug. We reset and report `DeviceReset` instead of a false
        // success, so the caller knows the session is gone and must reopen.
        //
        // We deliberately do NOT reopen here. Post-reset, these devices need a
        // brief span of *quiet* (no USB traffic) to finish tearing the old
        // session down; a reopen issued immediately gets `SessionAlreadyOpen`,
        // and hammering close/open at it keeps it busy and re-wedges it. So
        // reopen timing is the caller's job: on `DeviceReset`, drop the device,
        // wait a beat, and open again (see the AGENTS.md cancellation notes).
        if wedged {
            diag_debug!(
                "cancel_transfer: step 5 device wedged after cancel (#18), issuing DEVICE_RESET; \
                 returning DeviceReset (caller must reopen quietly)"
            );
            // Best effort: a device too stuck even for the session-less reset
            // (some cameras, #12) still needs a physical replug, but we surface
            // DeviceReset either way so the caller knows the session is gone.
            let reset = self.reset_device().await;
            diag_debug!("cancel_transfer: DEVICE_RESET result ok={}", reset.is_ok());
            return Err(crate::PtpError::DeviceReset);
        }

        diag_debug!("cancel_transfer: complete, session healthy");
        Ok(())
    }

    /// USB Still Image Class device reset.
    ///
    /// Sequence:
    /// 1. Send DEVICE_RESET control request (bRequest=0x66, no payload). The
    ///    device returns to the Idle state and abandons any half-finished
    ///    transaction.
    /// 2. Clear halts on both bulk endpoints. The reset resets the device's
    ///    data toggles, so the host side must resync or the next transfers
    ///    silently drop.
    /// 3. Drain stale bulk IN data. Leftover containers from an aborted
    ///    transfer otherwise surface in the next session as "Transaction ID
    ///    mismatch" or "expected Response container type (3), got 2"
    ///    (observed on the Panasonic Lumix DMC-TZ61, issue #12, after a
    ///    Ctrl-C'd listing).
    async fn reset_device(&self) -> Result<(), crate::PtpError> {
        diag_debug!("reset_device: sending SIC DEVICE_RESET (0x66)");
        // Step 1: DEVICE_RESET control request.
        self.interface
            .control_out(
                ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: SIC_DEVICE_RESET_REQUEST,
                    value: 0,
                    index: self.interface_number as u16,
                    data: &[],
                },
                Duration::from_secs(1),
            )
            .await
            .map_err(Self::convert_transfer_error)?;

        // Step 2: Resync both bulk endpoints (consume pending, clear halt).
        {
            let mut ep = self.bulk_out.lock().await;
            if ep.pending() > 0 {
                ep.cancel_all();
                while ep.pending() > 0 {
                    let _ = ep.next_complete().await;
                }
            }
            let _ = blocking(ep.clear_halt());
        }
        {
            let mut ep = self.bulk_in.lock().await;
            if ep.pending() > 0 {
                ep.cancel_all();
                while ep.pending() > 0 {
                    let _ = ep.next_complete().await;
                }
            }
            let _ = blocking(ep.clear_halt());

            // Step 3: Drain stale bulk IN data until the pipe is idle.
            let max_packet_size = ep.max_packet_size();
            loop {
                if ep.pending() == 0 {
                    ep.submit(Buffer::new(align_to_packet_size(
                        max_packet_size,
                        max_packet_size,
                    )));
                }
                let got_data = {
                    let complete_fut = ep.next_complete();
                    let timeout_fut = Delay::new(Duration::from_millis(300));
                    futures::pin_mut!(complete_fut, timeout_fut);
                    match futures::future::select(complete_fut, timeout_fut).await {
                        futures::future::Either::Left((completion, _)) => completion.status.is_ok(),
                        futures::future::Either::Right((_, _)) => false,
                    }
                };
                if !got_data {
                    ep.cancel_all();
                    while ep.pending() > 0 {
                        let _ = ep.next_complete().await;
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    async fn drain_bulk_in(
        &self,
        idle_timeout: Duration,
        max_duration: Duration,
    ) -> Result<usize, crate::PtpError> {
        let mut ep = self.bulk_in.lock().await;
        // The leftover can be the rest of a whole window, so read big, like a refused cancel.
        let read_size = align_to_packet_size(Self::REFUSED_CANCEL_DRAIN_READ, ep.max_packet_size());
        let deadline = std::time::Instant::now() + max_duration;
        let mut drained = 0usize;
        let result = loop {
            if std::time::Instant::now() >= deadline {
                break Ok(drained);
            }
            if ep.pending() == 0 {
                ep.submit(Buffer::new(read_size));
            }
            let completion = {
                let complete_fut = ep.next_complete();
                let timeout_fut = Delay::new(idle_timeout);
                futures::pin_mut!(complete_fut, timeout_fut);
                match futures::future::select(complete_fut, timeout_fut).await {
                    futures::future::Either::Left((completion, _)) => Some(completion),
                    futures::future::Either::Right((_, _)) => None,
                }
            };
            match completion {
                None => break Ok(drained),
                Some(c) => match c.status {
                    Ok(()) => drained += c.actual_len,
                    Err(e) if is_disconnect(e) => break Err(crate::PtpError::Disconnected),
                    Err(e) => {
                        let _ = clear_halt_if_stalled(&mut ep, e);
                        break Ok(drained);
                    }
                },
            }
        };
        ep.cancel_all();
        while ep.pending() > 0 {
            let _ = ep.next_complete().await;
        }
        result
    }
}

/// Round `size` up to the nearest multiple of `packet_size`.
///
/// nusb 0.2 requires that IN transfer buffer sizes are non-zero multiples of
/// the endpoint's maximum packet size.
fn align_to_packet_size(size: usize, packet_size: usize) -> usize {
    if packet_size == 0 {
        return size.max(1);
    }
    if size == 0 {
        return packet_size;
    }
    if size.is_multiple_of(packet_size) {
        size
    } else {
        ((size / packet_size) + 1) * packet_size
    }
}

/// Whether one drained bulk read is the Response container that closes the transaction.
///
/// A response is 12–32 bytes and arrives as its own short read, with a length field
/// matching the read. Checking only the type code at bytes 4..6 isn't enough: a drain that
/// reads a whole object (a device that refused CLASS_CANCEL, #43) sees `03 00` there in
/// file data roughly once per 65,536 reads, and would stop with the rest still in the pipe.
fn is_response_container(read: &[u8], requested: usize) -> bool {
    if read.len() < 12 || read.len() >= requested {
        return false;
    }
    let length = u32::from_le_bytes([read[0], read[1], read[2], read[3]]) as usize;
    let type_code = u16::from_le_bytes([read[4], read[5]]);
    length == read.len() && type_code == 3
}

/// Derive a stable location identifier from USB topology (bus + port chain).
///
/// Uses FNV-1a to hash `bus_id` and `port_chain` into a deterministic `u64`.
/// The result is stable across calls for the same physical USB port, regardless
/// of which device is plugged in.
fn location_id_from_topology(dev: &nusb::DeviceInfo) -> u64 {
    // FNV-1a 64-bit constants
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in dev.bus_id().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    // Separator so bus_id "1" + port [2,3] differs from bus_id "12" + port [3]
    hash ^= 0xFF;
    hash = hash.wrapping_mul(FNV_PRIME);
    for byte in dev.port_chain() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_bytes(code: u16, tx_id: u32, params: &[u32]) -> Vec<u8> {
        let len = 12 + 4 * params.len();
        let mut buf = Vec::with_capacity(len);
        buf.extend_from_slice(&(len as u32).to_le_bytes());
        buf.extend_from_slice(&3u16.to_le_bytes());
        buf.extend_from_slice(&code.to_le_bytes());
        buf.extend_from_slice(&tx_id.to_le_bytes());
        for p in params {
            buf.extend_from_slice(&p.to_le_bytes());
        }
        buf
    }

    #[test]
    fn disconnect_converts_to_disconnected() {
        assert!(is_disconnect(TransferError::Disconnected));
        assert!(matches!(
            NusbTransport::convert_transfer_error(TransferError::Disconnected),
            crate::PtpError::Disconnected
        ));
    }

    #[test]
    fn other_transfer_errors_stay_io() {
        for err in [
            TransferError::Stall,
            TransferError::Fault,
            TransferError::InvalidArgument,
            TransferError::Unknown(0x1234),
        ] {
            assert!(!is_disconnect(err), "{err:?}");
            assert!(matches!(
                NusbTransport::convert_transfer_error(err),
                crate::PtpError::Io(_)
            ));
        }
    }

    /// Pulling the cable mid-transfer on macOS fails the in-flight transfer with
    /// `kIOReturnNotResponding` and the next one with `kIOReturnNotOpen`, both of which
    /// nusb (0.2.8) leaves as `Unknown`. Both must read as a disconnect (#53).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_pulled_cable_codes_convert_to_disconnected() {
        for code in [0xe000_02ed, 0xe000_02cd] {
            let err = TransferError::Unknown(code);
            assert!(is_disconnect(err), "{code:#x}");
            assert!(matches!(
                NusbTransport::convert_transfer_error(err),
                crate::PtpError::Disconnected
            ));
        }
    }

    /// A Galaxy A57 that has gone silent lets CLASS_CANCEL time out. Returning that as
    /// `Timeout` skipped the wedge check, so the caller never learned to reopen (#43).
    #[test]
    fn unanswered_cancel_request_continues_like_a_refused_one() {
        assert!(!cancel_was_not_acted_on(Ok(())).unwrap());
        assert!(cancel_was_not_acted_on(Err(TransferError::Stall)).unwrap());
        assert!(cancel_was_not_acted_on(Err(TransferError::Cancelled)).unwrap());
    }

    #[test]
    fn cancel_request_failures_still_surface() {
        assert!(matches!(
            cancel_was_not_acted_on(Err(TransferError::Disconnected)),
            Err(crate::PtpError::Disconnected)
        ));
        assert!(matches!(
            cancel_was_not_acted_on(Err(TransferError::Fault)),
            Err(crate::PtpError::Io(_))
        ));
    }

    #[test]
    fn response_container_is_recognized() {
        assert!(is_response_container(&response_bytes(0x2001, 7, &[]), 512));
        assert!(is_response_container(
            &response_bytes(0x201F, 7, &[1, 2]),
            65_536
        ));
    }

    #[test]
    fn full_read_of_file_bytes_is_not_a_response() {
        // File data that happens to carry `03 00` at bytes 4..6. While a refused cancel
        // drains a whole object, some full-size read eventually does (#43).
        let mut read = vec![0xAB; 512];
        read[4] = 3;
        read[5] = 0;
        assert!(!is_response_container(&read, 512));
    }

    #[test]
    fn short_read_needs_a_matching_length_field() {
        // The tail of a data phase ends in a short read, and its bytes 4..6 can be `03 00` too.
        let mut read = vec![0xAB; 100];
        read[4] = 3;
        read[5] = 0;
        assert!(!is_response_container(&read, 512));
        assert!(!is_response_container(&[0x0C, 0, 0, 0, 3, 0], 512));
    }

    #[test]
    fn parse_device_status_ok() {
        // wLength=4, code=OK (0x2001), no params
        let data = [0x04, 0x00, 0x01, 0x20];
        assert_eq!(parse_device_status(&data), Some((0x2001, vec![])));
    }

    #[test]
    fn parse_device_status_busy() {
        // wLength=4, code=Device_Busy (0x2019)
        let data = [0x04, 0x00, 0x19, 0x20];
        assert_eq!(parse_device_status(&data), Some((0x2019, vec![])));
    }

    #[test]
    fn parse_device_status_with_stalled_endpoints() {
        // wLength=12, code=Transaction_Cancelled (0x201F), params carry the
        // stalled endpoint addresses 0x81 (bulk IN) and 0x02 (bulk OUT).
        let data = [
            0x0C, 0x00, 0x1F, 0x20, // header
            0x81, 0x00, 0x00, 0x00, // param 1
            0x02, 0x00, 0x00, 0x00, // param 2
        ];
        assert_eq!(parse_device_status(&data), Some((0x201F, vec![0x81, 0x02])));
    }

    #[test]
    fn parse_device_status_respects_wlength_over_buffer_padding() {
        // Device says 8 bytes meaningful, host buffer has trailing garbage.
        let data = [
            0x08, 0x00, 0x01, 0x20, // header: wLength=8, OK
            0x81, 0x00, 0x00, 0x00, // param 1
            0xFF, 0xFF, 0xFF, 0xFF, // garbage past wLength: ignored
        ];
        assert_eq!(parse_device_status(&data), Some((0x2001, vec![0x81])));
    }

    #[test]
    fn parse_device_status_too_short() {
        assert_eq!(parse_device_status(&[]), None);
        assert_eq!(parse_device_status(&[0x04, 0x00, 0x01]), None);
    }

    #[test]
    fn usb_speed_from_nusb_maps_every_documented_variant() {
        assert_eq!(UsbSpeed::from_nusb(nusb::Speed::Low), Some(UsbSpeed::Low));
        assert_eq!(UsbSpeed::from_nusb(nusb::Speed::Full), Some(UsbSpeed::Full));
        assert_eq!(UsbSpeed::from_nusb(nusb::Speed::High), Some(UsbSpeed::High));
        assert_eq!(
            UsbSpeed::from_nusb(nusb::Speed::Super),
            Some(UsbSpeed::Super)
        );
        assert_eq!(
            UsbSpeed::from_nusb(nusb::Speed::SuperPlus),
            Some(UsbSpeed::SuperPlus)
        );
    }

    #[test]
    fn usb_speed_is_round_trip_safe() {
        // The five tiers are distinct and ordered so consumers can compare directly.
        let tiers = [
            UsbSpeed::Low,
            UsbSpeed::Full,
            UsbSpeed::High,
            UsbSpeed::Super,
            UsbSpeed::SuperPlus,
        ];
        for (a, b) in tiers.iter().zip(tiers.iter().skip(1)) {
            assert_ne!(a, b);
        }
    }

    #[test]
    fn summary_match_accepts_garmin_style_mtp_interface_string() {
        assert_eq!(
            NusbTransport::mtp_summary_match_reason(0xff, 0xff, 0x00, Some("MTP")),
            Some(MtpMatchReason::InterfaceString)
        );
    }

    #[test]
    fn summary_match_rejects_vendor_specific_without_mtp_string() {
        assert_eq!(
            NusbTransport::mtp_summary_match_reason(0xff, 0xff, 0x00, None),
            None
        );
        assert_eq!(
            NusbTransport::mtp_summary_match_reason(0xff, 0xff, 0x00, Some("Vendor")),
            None
        );
    }

    #[test]
    fn summary_match_accepts_standard_class_before_interface_string() {
        assert_eq!(
            NusbTransport::mtp_summary_match_reason(0x06, 0x01, 0x01, Some("Camera")),
            Some(MtpMatchReason::StandardClass)
        );
    }

    #[test]
    fn match_reason_as_str_uses_stable_snake_case_values() {
        assert_eq!(MtpMatchReason::StandardClass.as_str(), "standard_class");
        assert_eq!(MtpMatchReason::InterfaceString.as_str(), "interface_string");
        assert_eq!(MtpMatchReason::KnownVidPid.as_str(), "known_vid_pid");
        assert_eq!(
            MtpMatchReason::OpenedDescriptorScan.as_str(),
            "opened_descriptor_scan"
        );
    }

    #[test]
    #[ignore] // Requires real MTP device
    fn test_list_devices() {
        let devices = NusbTransport::list_mtp_devices().unwrap();
        println!("Found {} MTP devices", devices.len());
        for dev in &devices {
            println!(
                "  {:04x}:{:04x} serial={:?} location={:08x}",
                dev.vendor_id, dev.product_id, dev.serial_number, dev.location_id,
            );
        }
    }

    #[tokio::test]
    #[ignore] // Requires real MTP device
    async fn test_open_device() {
        let devices = NusbTransport::list_mtp_devices().unwrap();
        assert!(!devices.is_empty(), "No MTP device found");

        let device = devices[0].open().unwrap();
        let transport = NusbTransport::open(device).await.unwrap();

        assert_eq!(transport.timeout(), NusbTransport::DEFAULT_TIMEOUT);
    }

    #[tokio::test]
    #[ignore] // Requires real MTP device
    async fn test_timeout_configuration() {
        let devices = NusbTransport::list_mtp_devices().unwrap();
        assert!(!devices.is_empty(), "No MTP device found");

        let device = devices[0].open().unwrap();
        let custom_timeout = Duration::from_secs(60);
        let mut transport = NusbTransport::open_with_timeout(device, custom_timeout)
            .await
            .unwrap();

        assert_eq!(transport.timeout(), custom_timeout);

        // Test setter
        let new_timeout = Duration::from_secs(10);
        transport.set_timeout(new_timeout);
        assert_eq!(transport.timeout(), new_timeout);
    }

    #[test]
    fn test_align_to_packet_size() {
        // Zero size rounds up to packet_size
        assert_eq!(align_to_packet_size(0, 512), 512);
        // Size smaller than packet rounds up
        assert_eq!(align_to_packet_size(1, 512), 512);
        // Exact multiple stays the same
        assert_eq!(align_to_packet_size(512, 512), 512);
        assert_eq!(align_to_packet_size(1024, 512), 1024);
        // Non-multiple rounds up
        assert_eq!(align_to_packet_size(513, 512), 1024);
        assert_eq!(align_to_packet_size(100, 64), 128);
        // Zero packet_size edge case
        assert_eq!(align_to_packet_size(0, 0), 1);
        assert_eq!(align_to_packet_size(100, 0), 100);
    }

    #[test]
    fn test_mtp_class_detection() {
        // Image class with MTP subclass/protocol
        assert!(NusbTransport::is_mtp_class(0x06, 0x01, 0x01));

        // Vendor class with MTP subclass/protocol
        assert!(NusbTransport::is_mtp_class(0xFF, 0x01, 0x01));

        // Wrong class
        assert!(!NusbTransport::is_mtp_class(0x08, 0x01, 0x01));

        // Wrong subclass
        assert!(!NusbTransport::is_mtp_class(0x06, 0x00, 0x01));

        // Wrong protocol
        assert!(!NusbTransport::is_mtp_class(0x06, 0x01, 0x00));

        // Vendor-specific with non-standard subclass/protocol (e.g. Kindle ff/ff/00)
        assert!(!NusbTransport::is_mtp_class(0xFF, 0xFF, 0x00));
    }
}
