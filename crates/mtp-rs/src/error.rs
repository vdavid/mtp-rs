//! Error types for mtp-rs.

use thiserror::Error;

/// A failure the OS reported for a USB operation.
///
/// Opaque, so that the USB library underneath can change without changing this API. Read it through
/// `Display`, or convert the [`PtpError`] into [`mtp::Error`](crate::Error), which sorts USB
/// failures into `ExclusiveAccess`, `PermissionDenied`, `Disconnected`, and so on.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct UsbError(nusb::Error);

impl UsbError {
    /// The USB library's own classification, for sorting into neutral errors.
    pub(crate) fn kind(&self) -> nusb::ErrorKind {
        self.0.kind()
    }
}

/// Whether a USB failure of this kind means something else holds the device.
///
/// `Busy` covers macOS `kIOReturnExclusiveAccess` and Linux `EBUSY`. Go by the kind, not the
/// message: nusb's Linux text is "interface is busy (errno 16)", which is easy to miss.
fn usb_kind_means_exclusive_access(kind: nusb::ErrorKind) -> bool {
    kind == nusb::ErrorKind::Busy
}

/// The main error type for mtp-rs operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PtpError {
    /// USB communication error
    #[error("USB error: {0}")]
    Usb(UsbError),

    /// Protocol-level error from device
    #[error("Protocol error: {code:?} during {operation:?}")]
    Protocol {
        /// The response code returned by the device.
        code: crate::ptp::ResponseCode,
        /// The operation that triggered the error.
        operation: crate::ptp::OperationCode,
    },

    /// Invalid data received from device
    #[error("Invalid data: {message}")]
    InvalidData {
        /// Description of what was invalid.
        message: String,
    },

    /// I/O error
    #[error("I/O error: {0}")]
    Io(std::io::Error),

    /// Operation timed out
    #[error("Operation timed out")]
    Timeout,

    /// Device was disconnected
    #[error("Device disconnected")]
    Disconnected,

    /// Session not open
    #[error("Session not open")]
    SessionNotOpen,

    /// No device found
    #[error("No MTP device found")]
    NoDevice,

    /// Operation cancelled
    #[error("Operation cancelled")]
    Cancelled,

    /// A string to send is longer than PTP can carry.
    ///
    /// PTP strings hold at most [`MAX_STRING_UNITS`](crate::ptp::MAX_STRING_UNITS) UTF-16 code
    /// units, so a longer file name (or other string) can't be sent at all. Raised before anything
    /// reaches the device, so the session stays usable.
    #[error("string is {units} UTF-16 code units long; PTP strings hold at most 254")]
    StringTooLong {
        /// The string's length in UTF-16 code units, not counting the null terminator.
        units: usize,
    },

    /// A transfer cancel wedged the device, and the transport was reset to
    /// recover it. The PTP session is gone; reopen the device to continue.
    ///
    /// Seen when cancelling or abandoning an in-flight read on an Android device
    /// (issue #18): the device stops responding, so `cancel_transfer` issues a
    /// USB `DEVICE_RESET` and reports this instead of a false success. Transfer
    /// size doesn't drive it; a 36-byte file is enough.
    ///
    /// **Don't treat this as the only wedge signature.** A Samsung reports it; a
    /// Pixel wedges the same way but the next operation simply hangs with no
    /// error at all (verified on a Pixel 9 Pro XL, macOS/nusb, 2026-07-20), so
    /// wrap operations in a timeout as well as matching this variant. See
    /// `docs/notes/android-wedges-and-the-reset-kill-switch.md`.
    #[error("device was reset to recover from a wedged cancel; reopen to continue")]
    DeviceReset,
}

impl PtpError {
    /// Wrap a failure from the USB library. Deliberately not a `From` impl: that would be a public
    /// trait impl naming an `nusb` type.
    pub(crate) fn from_nusb(e: nusb::Error) -> Self {
        PtpError::Usb(UsbError(e))
    }

    /// Create an invalid data error with a message.
    #[must_use]
    pub fn invalid_data(message: impl Into<String>) -> Self {
        PtpError::InvalidData {
            message: message.into(),
        }
    }

    /// Check if this is a retryable error.
    ///
    /// Retryable errors are transient and the operation may succeed if retried:
    /// - `DeviceBusy`: Device is temporarily busy
    /// - `Timeout`: Operation timed out but device may still be responsive
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            PtpError::Protocol {
                code: crate::ptp::ResponseCode::DeviceBusy,
                ..
            } | PtpError::Timeout
        )
    }

    /// Get the response code if this is a protocol error.
    #[must_use]
    pub fn response_code(&self) -> Option<crate::ptp::ResponseCode> {
        match self {
            PtpError::Protocol { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// Check if this error indicates another process has exclusive access to the device.
    ///
    /// This typically happens on macOS when `ptpcamerad` or another application
    /// has already claimed the USB interface, or on Linux when a kernel driver or
    /// another process holds it. Applications can use this to provide
    /// platform-specific guidance to users.
    ///
    /// Answers the same as [`mtp::Error::is_exclusive_access`](crate::Error::is_exclusive_access)
    /// on the converted error. Missing OS permission (Linux `udev` rules, for example) doesn't
    /// count: the remedy there is different, see
    /// [`mtp::Error::is_permission_denied`](crate::Error::is_permission_denied).
    ///
    /// # Example
    ///
    /// ```ignore
    /// match device.open().await {
    ///     Err(e) if e.is_exclusive_access() => {
    ///         // On macOS, likely ptpcamerad interference
    ///         // App can query IORegistry for UsbExclusiveOwner to get details
    ///         show_exclusive_access_help();
    ///     }
    ///     Err(e) => handle_other_error(e),
    ///     Ok(dev) => use_device(dev),
    /// }
    /// ```
    #[must_use]
    pub fn is_exclusive_access(&self) -> bool {
        matches!(self, PtpError::Usb(usb) if usb_kind_means_exclusive_access(usb.kind()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

    #[test]
    fn only_a_busy_usb_kind_means_exclusive_access() {
        // nusb can't build an `Error` from outside, so the test pins the classification itself.
        assert!(usb_kind_means_exclusive_access(nusb::ErrorKind::Busy));
        for kind in [
            nusb::ErrorKind::PermissionDenied,
            nusb::ErrorKind::Disconnected,
            nusb::ErrorKind::NotFound,
            nusb::ErrorKind::Unsupported,
            nusb::ErrorKind::Other,
        ] {
            assert!(!usb_kind_means_exclusive_access(kind), "{kind:?}");
        }
    }

    #[test]
    fn io_errors_agree_with_the_neutral_error() {
        // `PtpError::Io` only carries transfer faults (a stall, a fault), never a claim refusal,
        // whatever its text says. The neutral error has always read them that way.
        for message in [
            "could not be opened for exclusive access",
            "Device or resource busy",
        ] {
            let low = PtpError::Io(IoError::other(message));
            assert!(!low.is_exclusive_access(), "{message}");
            assert!(!crate::Error::from(low).is_exclusive_access(), "{message}");
        }

        let denied = PtpError::Io(IoError::new(
            ErrorKind::PermissionDenied,
            "Access is denied",
        ));
        assert!(!denied.is_exclusive_access());
        assert!(crate::Error::from(denied).is_permission_denied());
    }

    #[test]
    fn test_is_exclusive_access_false_for_other_errors() {
        assert!(!PtpError::Timeout.is_exclusive_access());
        assert!(!PtpError::Disconnected.is_exclusive_access());
        assert!(!PtpError::NoDevice.is_exclusive_access());
        assert!(!PtpError::invalid_data("some error").is_exclusive_access());

        let io_err = IoError::new(ErrorKind::NotFound, "device not found");
        assert!(!PtpError::Io(io_err).is_exclusive_access());
    }
}
