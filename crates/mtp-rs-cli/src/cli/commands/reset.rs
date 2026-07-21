//! `mtp-rs reset`: last-resort recovery of a stuck device without replugging it.
//!
//! Sends the USB Still Image Class Device Reset request at the transport
//! level, with no PTP session. This works precisely when the device's PTP
//! state machine is wedged (every command fails with "Transaction ID
//! mismatch" or "expected Response container type" errors, typically after a
//! host process died mid-transfer).
//!
//! **On Android it can make things worse.** The reset cancels Android's
//! outstanding FunctionFS endpoint read and breaks the endpoint; `MtpServer`
//! exits its read loop and never re-arms, while the USB device controller stays
//! `configured`, so the phone keeps enumerating and answers nothing until the
//! user physically replugs (verified on a healthy Pixel 9 Pro XL, macOS/nusb +
//! `adb logcat`, 2026-07-21). So it stays the right tool for a device that's
//! already unreachable, and the wrong first move otherwise: spaced reopens come
//! first. See `docs/notes/android-wedges-and-the-reset-kill-switch.md`.
//!
//! Uses `PtpDevice` instead of the `MtpDevice` selection in `cli::device`:
//! opening an `MtpDevice` runs OpenSession, which is exactly what a stuck
//! device can't answer. Virtual devices have no USB transport, so this
//! command only sees real USB devices.

use serde::Serialize;
use std::time::Duration;

use mtp_rs::ptp::PtpDevice;
use mtp_rs::transport::NusbTransport;

use crate::cli::args::Cli;
use crate::cli::error::{CliError, CliErrorKind};
use crate::cli::output::print_json;

#[derive(Debug, Serialize)]
struct ResetRow {
    reset: bool,
    /// Model name when the device answered a session-less GetDeviceInfo
    /// after the reset; `null` when it stayed silent.
    responding_model: Option<String>,
}

pub async fn run(cli: &Cli) -> Result<(), CliError> {
    let timeout = Duration::from_secs(cli.timeout);

    let device = if let Some(serial) = &cli.device {
        PtpDevice::open_by_serial_with_timeout(serial, timeout).await
    } else if let Some(location) = cli.location {
        PtpDevice::open_by_location_with_timeout(location, timeout).await
    } else {
        let devices = NusbTransport::list_mtp_devices()
            .map_err(|e| CliError::from_mtp("list devices", e.into(), cli.verbose))?;
        match devices.as_slice() {
            [] => {
                return Err(CliError::new(CliErrorKind::NoDevice, "no MTP device found"));
            }
            [device] => PtpDevice::open_by_location_with_timeout(device.location_id, timeout).await,
            _ => {
                return Err(CliError::new(
                    CliErrorKind::AmbiguousSelection,
                    "multiple MTP devices found; pass --device SERIAL or --location LOCATION",
                ));
            }
        }
    }
    .map_err(|e| CliError::from_mtp("open device", e.into(), cli.verbose))?;

    device
        .reset_device()
        .await
        .map_err(|e| CliError::from_mtp("reset device", e.into(), cli.verbose))?;

    // Verify with a session-less GetDeviceInfo; a device that answers this is
    // back in business. Failure here is informational, not an error: some
    // devices need a moment after a reset.
    let responding_model = device.get_device_info().await.ok().map(|info| info.model);

    let row = ResetRow {
        reset: true,
        responding_model,
    };
    if cli.json {
        return print_json(&row);
    }

    match &row.responding_model {
        Some(model) => println!("Reset OK, device responding: {}", model),
        None => println!(
            "Reset sent, but the device didn't answer GetDeviceInfo yet. \
             Give it a moment or replug if it stays silent."
        ),
    }
    Ok(())
}
