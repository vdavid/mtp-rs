//! What a reader needs to trust a set of numbers: which code, which device, which link, when.

use mtp_rs::mtp::MtpDevice;
use mtp_rs::UsbSpeed;
use serde::{Deserialize, Serialize};
use std::process::Command;

/// The library's manifest, embedded at compile time: the bench depends on mtp-rs by path, so
/// this is the code under test.
const MTP_RS_MANIFEST: &str = include_str!("../../../crates/mtp-rs/Cargo.toml");

/// The mtp-rs version this binary was built against.
pub fn mtp_rs_version() -> &'static str {
    manifest_version(MTP_RS_MANIFEST)
}

/// The first top-level `version = "..."` line, which is `[package]`'s in mtp-rs's manifest.
fn manifest_version(manifest: &str) -> &str {
    manifest
        .lines()
        .find_map(|l| l.strip_prefix("version = \"")?.split('"').next())
        .unwrap_or("unknown")
}

/// The run's context, printed above every report and saved in the JSON output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunInfo {
    /// Local date and time the run started, `YYYY-MM-DD HH:MM ±hh:mm`.
    pub date: String,
    /// Host OS and version, for example `macOS 27.0.1`.
    pub os: String,
    /// mtp-rs crate version under test.
    pub mtp_rs_version: String,
    /// Git commit of the checkout, with `-dirty` when tracked files had local changes.
    pub git_rev: String,
    /// The system libmtp the baseline linked against (`pkg-config --modversion libmtp`).
    pub libmtp_version: String,
    /// Device manufacturer and model, as the device reports them.
    pub device: String,
    /// Device firmware version (`DeviceInfo::device_version`).
    pub device_firmware: String,
    /// Negotiated USB link speed: the slowest of host port, cable, and device.
    pub usb_speed: String,
}

impl RunInfo {
    /// Collect everything, opening the first device briefly to read its identity and link speed.
    /// A failure to reach the device leaves those fields as `unknown`; the benchmark itself
    /// reports the real error when it connects.
    pub async fn collect() -> Self {
        let mut info = RunInfo {
            date: chrono::Local::now()
                .format("%Y-%m-%d %H:%M %:z")
                .to_string(),
            os: host_os(),
            mtp_rs_version: mtp_rs_version().to_string(),
            git_rev: git_rev(),
            libmtp_version: command_output("pkg-config", &["--modversion", "libmtp"])
                .unwrap_or_else(|| "unknown".to_string()),
            device: "unknown".to_string(),
            device_firmware: "unknown".to_string(),
            usb_speed: "unknown".to_string(),
        };
        match probe_device().await {
            Ok((device, firmware, speed)) => {
                info.device = device;
                info.device_firmware = firmware;
                info.usb_speed = speed;
            }
            Err(e) => eprintln!("Couldn't read the device's identity: {e}"),
        }
        info
    }

    /// One line per fact, for the terminal and Markdown reports.
    pub fn lines(&self) -> Vec<String> {
        vec![
            format!("Date:       {}", self.date),
            format!("Host OS:    {}", self.os),
            format!("mtp-rs:     {} ({})", self.mtp_rs_version, self.git_rev),
            format!("libmtp:     {}", self.libmtp_version),
            format!(
                "Device:     {} (firmware {})",
                self.device, self.device_firmware
            ),
            format!("USB link:   {}", self.usb_speed),
        ]
    }
}

/// Open the device `MtpDevice::open_first` picks and match it to its listing entry, which is
/// where the link speed lives.
async fn probe_device() -> Result<(String, String, String), mtp_rs::Error> {
    let listed = MtpDevice::list_devices()?;
    let device = MtpDevice::open_first().await?;
    let di = device.device_info();
    let name = format!("{} {}", di.manufacturer, di.model);
    let firmware = di.device_version.clone();
    let entry = listed
        .iter()
        .find(|d| d.serial_number.as_deref() == Some(di.serial_number.as_str()))
        .or_else(|| (listed.len() == 1).then(|| &listed[0]));
    let speed = match entry {
        Some(d) => usb_speed_label(d.speed),
        None => "unknown (couldn't match the open device to a USB listing)".to_string(),
    };
    device.close().await?;
    Ok((name, firmware, speed))
}

/// A link speed with the marketing name people know it by and its signaling rate.
pub fn usb_speed_label(speed: Option<UsbSpeed>) -> String {
    match speed {
        Some(UsbSpeed::Low) => "USB 1.0 low speed (1.5 Mbit/s)".to_string(),
        Some(UsbSpeed::Full) => "USB 1.1 full speed (12 Mbit/s)".to_string(),
        Some(UsbSpeed::High) => "USB 2.0 high speed (480 Mbit/s)".to_string(),
        Some(UsbSpeed::Super) => "USB 3.2 Gen 1 (5 Gbit/s)".to_string(),
        Some(UsbSpeed::SuperPlus) => "USB 3.2 Gen 2 (10 Gbit/s)".to_string(),
        Some(other) => format!("{other:?}"),
        None => "unknown (the OS didn't report it)".to_string(),
    }
}

fn host_os() -> String {
    let detail = match std::env::consts::OS {
        "macos" => command_output("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}")),
        "windows" => command_output("cmd", &["/C", "ver"]),
        _ => command_output("uname", &["-sr"]),
    };
    detail.unwrap_or_else(|| std::env::consts::OS.to_string())
}

/// The checkout's commit, from the bench crate's own directory so it works from any cwd.
fn git_rev() -> String {
    let dir = env!("CARGO_MANIFEST_DIR");
    let Some(rev) = command_output("git", &["-C", dir, "rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".to_string();
    };
    let dirty = Command::new("git")
        .args(["-C", dir, "status", "--porcelain", "--untracked-files=no"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    if dirty {
        format!("{rev}-dirty")
    } else {
        rev
    }
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comes_from_the_package_section() {
        let manifest = "[package]\nname = \"x\"\nversion = \"1.2.3\"\n\n[dependencies]\nfoo = { version = \"9\" }\n";
        assert_eq!(manifest_version(manifest), "1.2.3");
        assert_eq!(manifest_version("[package]\nname = \"x\"\n"), "unknown");
        let v = mtp_rs_version();
        assert!(v.split('.').count() >= 3, "{v}");
    }

    #[test]
    fn speed_labels_name_the_generation() {
        assert_eq!(
            usb_speed_label(Some(UsbSpeed::High)),
            "USB 2.0 high speed (480 Mbit/s)"
        );
        assert_eq!(
            usb_speed_label(Some(UsbSpeed::Super)),
            "USB 3.2 Gen 1 (5 Gbit/s)"
        );
        assert!(usb_speed_label(None).starts_with("unknown"));
    }

    #[test]
    fn lines_carry_every_field() {
        let info = RunInfo {
            date: "2026-11-11 10:00 +01:00".into(),
            os: "macOS 27.0.1".into(),
            mtp_rs_version: "0.38.0".into(),
            git_rev: "abc123".into(),
            libmtp_version: "1.1.23".into(),
            device: "Google Pixel 9 Pro XL".into(),
            device_firmware: "BP4A".into(),
            usb_speed: "USB 3.2 Gen 1 (5 Gbit/s)".into(),
        };
        let all = info.lines().join("\n");
        for needle in [
            "2026-11-11",
            "macOS 27.0.1",
            "0.38.0 (abc123)",
            "1.1.23",
            "Pixel 9 Pro XL (firmware BP4A)",
            "5 Gbit/s",
        ] {
            assert!(all.contains(needle), "missing {needle} in\n{all}");
        }
    }
}
