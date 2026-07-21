//! Error types for mtp-rs.

use crate::ptp::ObjectHandle;
use thiserror::Error;

/// The main error type for mtp-rs operations.
#[derive(Debug, Error)]
pub enum PtpError {
    /// USB communication error
    #[error("USB error: {0}")]
    Usb(#[from] nusb::Error),

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

/// Error from an upload, carrying the handle of the object the device created
/// during `SendObjectInfo` before the data phase failed.
///
/// PTP uploads are two-phase: `SendObjectInfo` creates the object on the device
/// (returning a handle), then `SendObject` streams the bytes. If the data phase
/// fails or is cancelled, the device is left holding a partial (often empty or
/// truncated) object. This error surfaces that handle so the caller owns the
/// cleanup-or-resume decision, rather than the library guessing.
///
/// The library does **not** auto-delete the partial object: deleting it would
/// issue hidden USB I/O to a possibly-disconnected device, the leave-vs-delete
/// behavior is device-dependent, and PTP's two-phase model is designed so a
/// failed `SendObject` can be retried against the same handle (resume).
///
/// [`From<PtpUploadError> for PtpError`] keeps `?` ergonomic for callers working in a
/// [`enum@PtpError`] context; they drop the [`partial`](Self::partial) handle unless
/// they match on `PtpUploadError` explicitly.
///
/// This is the low-level PTP-layer upload error. The high-level [`crate::mtp`] API
/// has its own backend-neutral [`crate::mtp::UploadError`].
#[derive(Debug, Error)]
#[error("{source}")]
pub struct PtpUploadError {
    /// The underlying failure (I/O, protocol, cancellation, timeout, …).
    #[source]
    pub source: PtpError,
    /// The handle of the partially-written object the device may still hold.
    ///
    /// `Some` iff `SendObjectInfo` succeeded but the data phase did not complete
    /// (genuine error OR cancellation). The object may be empty or truncated. The
    /// caller decides: delete it to discard the corrupt artifact, or retry the
    /// data phase to resume.
    ///
    /// `None` iff no object was created (for example, `SendObjectInfo` itself
    /// failed because the storage is read-only or the parent is invalid).
    pub partial: Option<ObjectHandle>,
}

impl From<PtpUploadError> for PtpError {
    fn from(e: PtpUploadError) -> Self {
        e.source
    }
}

impl PtpError {
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
    /// has already claimed the USB interface. Applications can use this to provide
    /// platform-specific guidance to users.
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
        match self {
            PtpError::Usb(io_err) => {
                let msg = io_err.to_string().to_lowercase();
                // macOS: "could not be opened for exclusive access"
                // Linux: typically EBUSY, but message varies
                // Windows: "access denied" or similar
                msg.contains("exclusive access")
                    || msg.contains("device or resource busy")
                    || (msg.contains("access") && msg.contains("denied"))
            }
            PtpError::Io(io_err) => {
                let msg = io_err.to_string().to_lowercase();
                msg.contains("exclusive access")
                    || msg.contains("device or resource busy")
                    || (msg.contains("access") && msg.contains("denied"))
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

    #[test]
    fn test_is_exclusive_access_macos_message() {
        // macOS nusb error message (tested via Io variant; same logic as Usb variant)
        let io_err = IoError::other("could not be opened for exclusive access");
        let err = PtpError::Io(io_err);
        assert!(err.is_exclusive_access());
    }

    #[test]
    fn test_is_exclusive_access_linux_busy() {
        // Linux EBUSY style message (tested via Io variant; same logic as Usb variant)
        let io_err = IoError::other("Device or resource busy");
        let err = PtpError::Io(io_err);
        assert!(err.is_exclusive_access());
    }

    #[test]
    fn test_is_exclusive_access_windows_denied() {
        // Windows access denied style message (tested via Io variant; same logic as Usb variant)
        let io_err = IoError::new(ErrorKind::PermissionDenied, "Access is denied");
        let err = PtpError::Io(io_err);
        assert!(err.is_exclusive_access());
    }

    #[test]
    fn test_is_exclusive_access_io_error() {
        // Also works for Io variant
        let io_err = IoError::other("could not be opened for exclusive access");
        let err = PtpError::Io(io_err);
        assert!(err.is_exclusive_access());
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
