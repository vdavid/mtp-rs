//! Watch for MTP devices being plugged in and unplugged.
//!
//! This module is private; its contents are re-exported from [`crate::mtp`], so the published
//! contract lives on [`DeviceWatch`] rather than here.

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{FutureExt, Stream, StreamExt};

use crate::mtp::{Error, MtpDeviceInfo};
use crate::transport::NusbTransport;

/// How long to wait after a USB event before enumerating, by default.
///
/// A device is not necessarily ready to describe itself the instant the OS announces it: its
/// interface descriptors can still be unpopulated, in which case it doesn't look like an MTP
/// device yet and would be silently skipped. Waiting a beat and then asking the OS afresh is what
/// makes arrival detection reliable. Override with [`DeviceWatchBuilder::settle_delay`].
pub const DEFAULT_SETTLE_DELAY: Duration = Duration::from_millis(500);

/// A device was plugged in or unplugged.
///
/// Only MTP devices produce events; see [`DeviceWatch`] for what the stream guarantees.
///
/// Deliberately not `#[non_exhaustive]`: a device is either here or it isn't, and there's no third
/// state to grow into. Consumers get an exhaustive `match` instead of a dead wildcard arm. The
/// payload carries [`MtpDeviceInfo`], which *is* `#[non_exhaustive]`, so new device facts still land
/// without breaking anyone.
#[derive(Debug, Clone)]
pub enum HotplugEvent {
    /// A device appeared, or was already present when watching began.
    Arrived(MtpDeviceInfo),
    /// A device went away. The info is what the watch last saw for it.
    Left(MtpDeviceInfo),
}

/// Identity of a device, used to tell "same device, still there" from "different device now".
///
/// Keyed on the USB topology position, because that's the one field the OS reports on both connect
/// and disconnect. The rest of the tuple detects a swap: unplug one phone, plug another into the
/// same port between two enumerations, and the position alone would call it unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DeviceKey {
    location_id: u64,
    vendor_id: u16,
    product_id: u16,
    serial_number: Option<String>,
}

impl DeviceKey {
    fn of(info: &MtpDeviceInfo) -> Self {
        Self {
            location_id: info.location_id,
            vendor_id: info.vendor_id,
            product_id: info.product_id,
            serial_number: info.serial_number.clone(),
        }
    }
}

/// Compare the devices present now against the ones last seen, and report what changed.
///
/// Pure, so the interesting behavior is testable without USB hardware. Updates `known` in place.
fn diff(
    known: &mut HashMap<DeviceKey, MtpDeviceInfo>,
    current: Vec<MtpDeviceInfo>,
) -> Vec<HotplugEvent> {
    let current: HashMap<DeviceKey, MtpDeviceInfo> = current
        .into_iter()
        .map(|i| (DeviceKey::of(&i), i))
        .collect();

    // Departures first: a device that changed identity in place should read as "the old one left,
    // then the new one arrived", not the other way around.
    let mut events: Vec<HotplugEvent> = known
        .iter()
        .filter(|(key, _)| !current.contains_key(key))
        .map(|(_, info)| HotplugEvent::Left(info.clone()))
        .collect();

    events.extend(
        current
            .iter()
            .filter(|(key, _)| !known.contains_key(key))
            .map(|(_, info)| HotplugEvent::Arrived(info.clone())),
    );

    *known = current;
    events
}

/// Configure a [`DeviceWatch`] before starting it.
///
/// Only needed to widen device matching or to tune the settle delay; most callers want
/// [`watch_devices`].
#[derive(Debug, Clone)]
pub struct DeviceWatchBuilder {
    known_devices: Vec<(u16, u16)>,
    settle_delay: Duration,
}

impl Default for DeviceWatchBuilder {
    fn default() -> Self {
        Self {
            known_devices: Vec::new(),
            settle_delay: DEFAULT_SETTLE_DELAY,
        }
    }
}

impl DeviceWatchBuilder {
    /// Start configuring a watch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Also report devices matching these VID/PID pairs, even when their USB descriptors don't
    /// carry the standard MTP class codes.
    ///
    /// Mirrors
    /// [`MtpDevice::list_devices_with_known`](crate::mtp::MtpDevice::list_devices_with_known); pass
    /// the same list to both so a device you can list is also a device you can watch for.
    #[must_use]
    pub fn known_devices(mut self, known: &[(u16, u16)]) -> Self {
        self.known_devices = known.to_vec();
        self
    }

    /// How long to wait after a USB event before enumerating (default
    /// [`DEFAULT_SETTLE_DELAY`]).
    ///
    /// Lower it to react faster at the risk of missing a device that hasn't finished describing
    /// itself; raise it for devices that are slow to enumerate. `Duration::ZERO` enumerates
    /// immediately.
    #[must_use]
    pub fn settle_delay(mut self, delay: Duration) -> Self {
        self.settle_delay = delay;
        self
    }

    /// Start watching.
    ///
    /// Enumerates once up front, so every device already connected is reported as
    /// [`HotplugEvent::Arrived`] when the stream is first polled.
    ///
    /// # Errors
    ///
    /// Returns an error if the OS refuses to set up USB hotplug notifications.
    pub fn watch(self) -> Result<DeviceWatch, Error> {
        let usb = nusb::watch_devices().map_err(crate::PtpError::from_nusb)?;
        Ok(DeviceWatch {
            usb,
            known: HashMap::new(),
            known_devices: self.known_devices,
            settle_delay: self.settle_delay,
            pending: Vec::new(),
            settling: None,
            started: false,
        })
    }
}

/// A stream of [`HotplugEvent`]s reporting MTP devices arriving and leaving.
///
/// Build one with [`watch_devices`] or [`DeviceWatchBuilder`]. Only devices that
/// [`MtpDevice::list_devices`](crate::mtp::MtpDevice::list_devices) would list produce events; mice,
/// hubs, and chargers never reach the consumer.
///
/// ```rust,no_run
/// use futures::StreamExt;
/// use mtp_rs::mtp::{watch_devices, HotplugEvent, MtpDevice};
///
/// # async fn example() -> Result<(), mtp_rs::Error> {
/// let mut watch = watch_devices()?;
/// while let Some(event) = watch.next().await {
///     match event {
///         HotplugEvent::Arrived(info) => {
///             let serial = info.serial_number.clone().unwrap_or_default();
///             let device = MtpDevice::open_by_serial(&serial).await?;
///             println!("mounted {}", device.device_info().model);
///         }
///         HotplugEvent::Left(info) => println!("gone: {:?}", info.serial_number),
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// # What the stream guarantees
///
/// - **Devices already plugged in when watching starts arrive as [`HotplugEvent::Arrived`]**, before
///   any live event. So a consumer needs one code path, not an enumerate-then-watch pair, and a
///   device plugged in during startup can't slip through the gap between the two. Consumers must not
///   enumerate separately as well, or they'll count those devices twice.
/// - **[`HotplugEvent::Left`] carries the full [`MtpDeviceInfo`]** of the device that went away,
///   including its serial. The OS reports only an opaque id on disconnect, so the watch caches what
///   it last saw; without that a consumer can't tell which of two phones was unplugged.
/// - **A device that changes identity in place** (a phone switching from charge-only to file
///   transfer, which re-enumerates it with new descriptors) is reported as a `Left` for the old
///   identity followed by an `Arrived` for the new one.
/// - **The stream never ends on its own**; it stays live until dropped. Dropping it stops the
///   notifications and any later device changes are missed, so a consumer tracking devices for the
///   lifetime of a process should hold it for that long.
///
/// # Scope
///
/// USB only. Virtual devices (feature `virtual-device`) are registered in-process rather than
/// plugged in, so they never produce hotplug events even though `list_devices` includes them.
///
/// The watch reports device presence; it does not open anything. A device that has just arrived may
/// still refuse to open a session for a moment, and on Android the user may not have granted
/// file-transfer mode yet. Retry with backoff rather than treating the first failure as fatal.
pub struct DeviceWatch {
    usb: nusb::hotplug::HotplugWatch,
    known: HashMap<DeviceKey, MtpDeviceInfo>,
    known_devices: Vec<(u16, u16)>,
    settle_delay: Duration,
    /// Events produced by the last enumeration, not yet handed to the consumer.
    pending: Vec<HotplugEvent>,
    /// In-flight settle delay; enumeration happens when it completes.
    settling: Option<BoxFuture<'static, ()>>,
    started: bool,
}

impl std::fmt::Debug for DeviceWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceWatch")
            .field("known", &self.known.len())
            .field("settle_delay", &self.settle_delay)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl DeviceWatch {
    /// Ask the OS what's connected now and turn the difference into events.
    ///
    /// Enumerating afresh rather than trusting the event's own device snapshot is deliberate: the
    /// snapshot can predate the device's descriptors being readable, and it says nothing about
    /// devices whose events were coalesced.
    fn enumerate(&mut self) {
        match NusbTransport::list_mtp_devices_with_known(&self.known_devices) {
            Ok(devices) => {
                let current = devices.into_iter().map(MtpDeviceInfo::from_usb).collect();
                self.pending.extend(diff(&mut self.known, current));
            }
            // A failed enumeration means "we don't know what's out there", not "nothing is out
            // there". Reporting every device as departed on a transient failure would be worse
            // than waiting for the next event, which re-enumerates anyway.
            Err(e) => {
                diag_debug!(
                    "hotplug enumeration failed, keeping last known device set: {}",
                    e
                );
            }
        }
    }
}

impl Stream for DeviceWatch {
    type Item = HotplugEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if !this.pending.is_empty() {
                return Poll::Ready(Some(this.pending.remove(0)));
            }

            // First poll: report what's already plugged in, with no settle delay. These devices
            // have been present since before watching began, so they've had their beat already.
            if !this.started {
                this.started = true;
                this.enumerate();
                continue;
            }

            if let Some(settle) = this.settling.as_mut() {
                match settle.poll_unpin(cx) {
                    Poll::Ready(()) => {
                        this.settling = None;
                        this.enumerate();
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            match this.usb.poll_next_unpin(cx) {
                // The event itself is only a trigger; `enumerate` is what decides whether anything
                // relevant changed, so connect and disconnect are handled identically. Coalescing
                // is free: further events during the settle delay fold into the one enumeration.
                Poll::Ready(Some(_)) => {
                    this.settling = Some(sleep(this.settle_delay).boxed());
                    continue;
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

async fn sleep(duration: Duration) {
    if !duration.is_zero() {
        futures_timer::Delay::new(duration).await;
    }
}

/// Watch for MTP devices being plugged in and unplugged.
///
/// Devices already connected are reported as [`HotplugEvent::Arrived`] when the stream is first
/// polled, so this is the only enumeration a consumer needs. See [`DeviceWatch`] for the full
/// contract, and [`DeviceWatchBuilder`] to widen matching or tune timing.
///
/// # Errors
///
/// Returns an error if the OS refuses to set up USB hotplug notifications.
///
/// # Example
///
/// ```rust,no_run
/// use futures::StreamExt;
/// use mtp_rs::mtp::{watch_devices, HotplugEvent};
///
/// # async fn example() -> Result<(), mtp_rs::Error> {
/// let mut watch = watch_devices()?;
/// while let Some(event) = watch.next().await {
///     match event {
///         HotplugEvent::Arrived(info) => println!("arrived: {:?}", info.product),
///         HotplugEvent::Left(info) => println!("left: {:?}", info.product),
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub fn watch_devices() -> Result<DeviceWatch, Error> {
    DeviceWatchBuilder::new().watch()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MtpMatchReason;

    fn info(location_id: u64, serial: Option<&str>) -> MtpDeviceInfo {
        MtpDeviceInfo {
            vendor_id: 0x18d1,
            product_id: 0x4ee1,
            manufacturer: Some("Google".into()),
            product: Some("Pixel 9 Pro XL".into()),
            serial_number: serial.map(String::from),
            location_id,
            speed: None,
            match_reason: MtpMatchReason::StandardClass,
        }
    }

    fn serials(events: &[HotplugEvent]) -> Vec<(&'static str, Option<String>)> {
        events
            .iter()
            .map(|e| match e {
                HotplugEvent::Arrived(i) => ("arrived", i.serial_number.clone()),
                HotplugEvent::Left(i) => ("left", i.serial_number.clone()),
            })
            .collect()
    }

    #[test]
    fn first_enumeration_reports_every_connected_device_as_arrived() {
        let mut known = HashMap::new();
        let events = diff(&mut known, vec![info(1, Some("a")), info(2, Some("b"))]);

        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| matches!(e, HotplugEvent::Arrived(_))));
        assert_eq!(known.len(), 2);
    }

    #[test]
    fn unchanged_device_set_produces_no_events() {
        let mut known = HashMap::new();
        diff(&mut known, vec![info(1, Some("a"))]);

        // Every USB event on the system triggers an enumeration, including ones from a mouse or a
        // hub. Those must not reach the consumer as spurious device churn.
        let events = diff(&mut known, vec![info(1, Some("a"))]);
        assert!(events.is_empty());
    }

    #[test]
    fn unplugged_device_is_reported_with_the_info_last_seen() {
        let mut known = HashMap::new();
        diff(&mut known, vec![info(1, Some("a")), info(2, Some("b"))]);

        let events = diff(&mut known, vec![info(1, Some("a"))]);

        // The OS reports only an opaque id on disconnect, so carrying the cached info is the whole
        // point: a consumer with two phones attached has to know which one went away.
        assert_eq!(events.len(), 1);
        match &events[0] {
            HotplugEvent::Left(i) => {
                assert_eq!(i.serial_number.as_deref(), Some("b"));
                assert_eq!(i.product.as_deref(), Some("Pixel 9 Pro XL"));
            }
            other => panic!("expected Left, got {other:?}"),
        }
        assert_eq!(known.len(), 1);
    }

    #[test]
    fn device_swapped_on_the_same_port_reports_left_before_arrived() {
        let mut known = HashMap::new();
        diff(&mut known, vec![info(1, Some("a"))]);

        // Same USB position, different phone. Keying on position alone would call this unchanged
        // and leave the consumer talking to the wrong device.
        let events = diff(&mut known, vec![info(1, Some("b"))]);

        assert_eq!(
            serials(&events),
            vec![
                ("left", Some("a".to_string())),
                ("arrived", Some("b".to_string()))
            ]
        );
    }

    #[test]
    fn device_re_enumerating_into_file_transfer_mode_reports_left_then_arrived() {
        let mut known = HashMap::new();
        let mut charging = info(1, Some("a"));
        charging.product_id = 0x4ee7; // charge-only composite
        diff(&mut known, vec![charging]);

        // An Android phone switching to file transfer comes back with a different product ID. The
        // consumer needs a fresh Arrived to know it can open a session now.
        let events = diff(&mut known, vec![info(1, Some("a"))]);

        assert_eq!(
            serials(&events),
            vec![
                ("left", Some("a".to_string())),
                ("arrived", Some("a".to_string()))
            ]
        );
    }

    #[test]
    fn devices_without_serials_are_distinguished_by_port() {
        let mut known = HashMap::new();
        let events = diff(&mut known, vec![info(1, None), info(2, None)]);

        // Cameras often report no serial. Two of the same model must not collapse into one entry.
        assert_eq!(events.len(), 2);
        assert_eq!(known.len(), 2);

        let events = diff(&mut known, vec![info(2, None)]);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], HotplugEvent::Left(_)));
    }

    #[test]
    fn all_devices_gone_reports_every_one_as_left() {
        let mut known = HashMap::new();
        diff(&mut known, vec![info(1, Some("a")), info(2, Some("b"))]);

        let events = diff(&mut known, vec![]);

        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| matches!(e, HotplugEvent::Left(_))));
        assert!(known.is_empty());
    }
}
