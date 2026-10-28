use mtp_rs::Error;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliErrorKind {
    Other,
    NoDevice,
    AmbiguousSelection,
    AccessDenied,
    RemotePath,
    Transfer,
    Verify,
}

impl CliErrorKind {
    #[must_use]
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Other => 1,
            Self::NoDevice => 2,
            Self::AmbiguousSelection => 3,
            Self::AccessDenied => 4,
            Self::RemotePath => 5,
            Self::Transfer => 6,
            Self::Verify => 7,
        }
    }
}

#[derive(Debug)]
pub struct CliError {
    kind: CliErrorKind,
    message: String,
    detail: Option<String>,
    help: Option<String>,
}

impl CliError {
    #[must_use]
    pub fn new(kind: CliErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: None,
            help: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    #[must_use]
    pub fn from_mtp(context: &str, error: Error, verbose: bool) -> Self {
        let detail = verbose.then(|| format!("{error:?}"));
        let mut cli_error = if is_macos_usb_user_client_denied(&error) {
            Self::new(
                CliErrorKind::AccessDenied,
                format!("{context}: macOS denied USB access for this process"),
            )
            .with_help(macos_usb_user_client_help())
        } else if error.is_exclusive_access() {
            Self::new(
                CliErrorKind::AccessDenied,
                format!("{context}: device is already in use"),
            )
            .with_help(exclusive_access_help())
        } else {
            match &error {
                Error::NoDevice => Self::new(CliErrorKind::NoDevice, "no MTP device found"),
                Error::Timeout => {
                    Self::new(CliErrorKind::Transfer, format!("{context}: timed out"))
                }
                Error::Busy => Self::new(
                    CliErrorKind::Transfer,
                    format!("{context}: device busy, try again"),
                ),
                Error::Disconnected => Self::new(
                    CliErrorKind::Transfer,
                    format!("{context}: device disconnected"),
                ),
                // A re-keyed/invalid handle or parent: the remote path no longer
                // resolves to the object the host cached.
                Error::StaleHandle | Error::NotFound => Self::new(
                    CliErrorKind::RemotePath,
                    format!("{context}: remote path not found"),
                ),
                Error::NameTooLong { units } => Self::new(
                    CliErrorKind::RemotePath,
                    format!(
                        "{context}: name is too long for MTP ({units} UTF-16 units, max {})",
                        mtp_rs::MAX_NAME_UNITS
                    ),
                ),
                Error::StorageFull => Self::new(
                    CliErrorKind::Transfer,
                    format!("{context}: storage is full"),
                ),
                Error::AccessDenied => Self::new(
                    CliErrorKind::AccessDenied,
                    format!("{context}: storage is read-only or access denied"),
                ),
                Error::Unsupported => Self::new(
                    CliErrorKind::Other,
                    format!("{context}: operation not supported by this device"),
                ),
                Error::Cancelled => {
                    Self::new(CliErrorKind::Transfer, format!("{context}: cancelled"))
                }
                Error::InvalidData { message } => Self::new(
                    CliErrorKind::Transfer,
                    format!("{context}: invalid data: {message}"),
                ),
                Error::Io { message } => Self::new(
                    CliErrorKind::Other,
                    format!("{context}: I/O error: {message}"),
                ),
                _ => Self::new(CliErrorKind::Other, format!("{context}: {error}")),
            }
        };

        if let Some(detail) = detail {
            cli_error = cli_error.with_detail(detail);
        }
        cli_error
    }

    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.kind.exit_code()
    }

    #[must_use]
    pub fn help(&self) -> Option<&str> {
        self.help.as_deref()
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{} ({detail})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for CliError {}

fn exclusive_access_help() -> &'static str {
    "Close other applications that may own the USB device, such as Photos, Android File Transfer, Garmin Express, or another file manager."
}

fn macos_usb_user_client_help() -> &'static str {
    "macOS can deny USB user-client access to non-app or background-launched processes. Try running mtp-rs from Terminal/iTerm with the Mac unlocked and the accessory allowed, or launch it from a signed app/helper context."
}

fn is_macos_usb_user_client_denied(error: &Error) -> bool {
    #[cfg(target_os = "macos")]
    {
        // The neutral error folds USB faults into `Io { message }`, keeping the
        // underlying nusb message text. The IOKit user-client denial surfaces as
        // a failure to create the IOKit PlugInInterface, so match on that text.
        match error {
            Error::Io { message } => message.contains("failed to create IOKit PlugInInterface"),
            _ => false,
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = error;
        false
    }
}
