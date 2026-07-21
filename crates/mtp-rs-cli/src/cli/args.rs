use clap::{Args, Parser, Subcommand};
use std::str::FromStr;

#[derive(Debug, Parser)]
#[command(
    name = "mtp-rs",
    about = "Universal MTP/PTP file transfer CLI",
    version
)]
pub struct Cli {
    /// Device serial number to open.
    #[arg(long, global = true, conflicts_with = "location")]
    pub device: Option<String>,

    /// USB location ID to open, as decimal or hex.
    #[arg(long, global = true, value_parser = parse_u64_hex_or_dec, conflicts_with = "device")]
    pub location: Option<u64>,

    /// Storage index or storage ID. Decimal indexes are tried first.
    #[arg(long, global = true)]
    pub storage: Option<String>,

    /// Include a non-standard MTP device by VID:PID.
    #[arg(long, global = true, value_parser = parse_vid_pid)]
    pub known: Vec<(u16, u16)>,

    /// USB transfer timeout in seconds.
    #[arg(long, global = true, default_value_t = 30)]
    pub timeout: u64,

    /// Emit machine-readable JSON where supported.
    #[arg(long, global = true)]
    pub json: bool,

    /// Include lower-level error detail.
    #[arg(long, short, global = true)]
    pub verbose: bool,

    /// Print device-protocol diagnostics to stderr, for bug reports (issue #18):
    /// the cancel/reset path, transaction codes, session recovery. Equivalent to
    /// `RUST_LOG=mtp_rs=debug`; a set `RUST_LOG` takes precedence. Use `-vv`-style
    /// depth via `RUST_LOG=mtp_rs=trace` for per-operation detail.
    #[arg(long, global = true)]
    pub trace: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List visible MTP devices.
    Devices,

    /// Show device and storage information.
    Info,

    /// List objects in a remote folder.
    Ls(LsArgs),

    /// Upload a local file to a remote path.
    Put(PutArgs),

    /// Download a remote file to a local path.
    Get(GetArgs),

    /// Create one remote folder.
    Mkdir(RemotePathArg),

    /// Delete a remote object.
    Rm(RmArgs),

    /// Rename a remote object in place.
    Rename(RenameArgs),

    /// Move a remote object to another folder or path.
    Mv(MoveArgs),

    /// Copy a remote object to another folder or path.
    Cp(CopyArgs),

    /// Diagnose device visibility and basic MTP access.
    Doctor(DoctorArgs),

    /// Last-resort reset of a stuck device's USB transport state (no PTP session needed).
    ///
    /// Try unplugging nothing and simply retrying first: wait a few seconds with the device idle,
    /// then run your command again, a few times with gaps. Reach for `reset` only when that has
    /// failed, or when the device is already unreachable.
    ///
    /// On Android this can break the phone's MTP function until the user physically replugs it: a
    /// healthy Pixel 9 Pro XL stopped answering for good after a reset, while still enumerating over
    /// USB (verified on a Pixel 9 Pro XL, macOS/nusb, 2026-07-21).
    Reset,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Also run the cancel-health probe: download a file, cancel mid-stream, and
    /// report whether the session survived or wedged (the #18 reproducer). It's
    /// read-only but transfers data and can briefly wedge a device (the library
    /// recovers it via a USB reset). Off by default so plain `doctor` stays
    /// passive; add it when investigating a mid-transfer freeze.
    #[arg(long)]
    pub probe_cancel: bool,

    /// Probe this exact remote file instead of searching for one, for example
    /// `--probe-path /DCIM/Camera/IMG_0001.jpg`. Implies `--probe-cancel`. Use
    /// it when the search picks a file you'd rather leave alone, or when the
    /// device keeps its files somewhere the depth-limited search misses.
    #[arg(long, value_name = "PATH")]
    pub probe_path: Option<String>,
}

#[derive(Debug, Args)]
pub struct LsArgs {
    /// Remote folder path.
    #[arg(default_value = "/")]
    pub remote_path: String,

    /// List descendants recursively.
    #[arg(long)]
    pub recursive: bool,
}

#[derive(Debug, Args)]
pub struct PutArgs {
    /// Local file path.
    pub local_path: std::path::PathBuf,

    /// Remote destination path.
    pub remote_path: String,

    /// Replace a visible existing remote file.
    #[arg(long)]
    pub replace: bool,

    /// Download back and compare bytes after upload.
    #[arg(long)]
    pub verify: bool,
}

#[derive(Debug, Args)]
pub struct GetArgs {
    /// Remote file path.
    pub remote_path: String,

    /// Local destination path.
    pub local_path: std::path::PathBuf,

    /// Replace an existing local file.
    #[arg(long)]
    pub replace: bool,
}

#[derive(Debug, Args)]
pub struct RemotePathArg {
    /// Remote path.
    pub remote_path: String,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Remote path.
    pub remote_path: String,

    /// Confirm deletion.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct RenameArgs {
    /// Remote object path.
    pub remote_path: String,

    /// New filename, not a path.
    pub new_name: String,
}

#[derive(Debug, Args)]
pub struct MoveArgs {
    /// Remote source path.
    pub source_path: String,

    /// Remote destination path or folder.
    pub destination_path: String,

    /// Replace a visible existing destination object.
    #[arg(long)]
    pub replace: bool,
}

#[derive(Debug, Args)]
pub struct CopyArgs {
    /// Remote source path.
    pub source_path: String,

    /// Remote destination path or folder.
    pub destination_path: String,

    /// Replace a visible existing destination object.
    #[arg(long)]
    pub replace: bool,
}

fn parse_vid_pid(input: &str) -> Result<(u16, u16), String> {
    let (vid, pid) = input
        .split_once(':')
        .ok_or_else(|| "expected VID:PID, for example 091e:0003".to_string())?;
    Ok((parse_u16_hex(vid)?, parse_u16_hex(pid)?))
}

fn parse_u16_hex(input: &str) -> Result<u16, String> {
    let trimmed = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
        .unwrap_or(input);
    u16::from_str_radix(trimmed, 16).map_err(|_| format!("invalid hex u16 value '{input}'"))
}

pub fn parse_u64_hex_or_dec(input: &str) -> Result<u64, String> {
    if let Some(hex) = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16)
            .map_err(|_| format!("invalid hex u64 value '{input}'"));
    }

    if input.chars().any(|c| matches!(c, 'a'..='f' | 'A'..='F')) {
        return u64::from_str_radix(input, 16)
            .map_err(|_| format!("invalid hex u64 value '{input}'"));
    }

    u64::from_str(input).map_err(|_| format!("invalid decimal u64 value '{input}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn commands_parse() {
        for args in [
            ["mtp-rs", "devices"].as_slice(),
            ["mtp-rs", "info"].as_slice(),
            ["mtp-rs", "ls", "/"].as_slice(),
            ["mtp-rs", "put", "local.bin", "/remote.bin"].as_slice(),
            ["mtp-rs", "get", "/remote.bin", "local.bin"].as_slice(),
            ["mtp-rs", "mkdir", "/Upload"].as_slice(),
            ["mtp-rs", "rm", "/Upload/old.bin", "--yes"].as_slice(),
            ["mtp-rs", "rename", "/Upload/old.bin", "new.bin"].as_slice(),
            ["mtp-rs", "mv", "/Upload/file.bin", "/Archive/file.bin"].as_slice(),
            ["mtp-rs", "cp", "/Upload/file.bin", "/Archive/file.bin"].as_slice(),
            ["mtp-rs", "doctor"].as_slice(),
        ] {
            Cli::try_parse_from(args.iter().copied()).unwrap();
        }
    }

    #[test]
    fn device_and_location_conflict() {
        assert!(
            Cli::try_parse_from(["mtp-rs", "--device", "serial", "--location", "0x1", "info"])
                .is_err()
        );
    }

    #[test]
    fn known_vid_pid_accepts_hex_pair() {
        assert_eq!(parse_vid_pid("091e:0003").unwrap(), (0x091e, 0x0003));
        assert_eq!(parse_vid_pid("0x091e:0x0003").unwrap(), (0x091e, 0x0003));
    }

    #[test]
    fn known_vid_pid_rejects_malformed_input() {
        assert!(parse_vid_pid("091e").is_err());
        assert!(parse_vid_pid("nope:0003").is_err());
    }

    #[test]
    fn location_parser_accepts_decimal_and_hex() {
        assert_eq!(parse_u64_hex_or_dec("42").unwrap(), 42);
        assert_eq!(parse_u64_hex_or_dec("0x2a").unwrap(), 42);
        assert_eq!(parse_u64_hex_or_dec("2a").unwrap(), 42);
    }
}
