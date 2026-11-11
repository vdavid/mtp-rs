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
    /// `DeviceInfo::device_version`. Android reports `1.0` here, not its build.
    pub device_firmware: String,
    /// Negotiated USB link speed: the slowest of host port, cable, and device.
    pub usb_speed: String,
    /// The serial in the device's MTP DeviceInfo, which is what libmtp reports, and can differ
    /// from the USB serial `--serial` takes (a Pixel 9 Pro XL gives a 32-digit hex one). Kept out of
    /// the report; used to check libmtp opened the same device.
    #[serde(skip)]
    pub mtp_serial: Option<String>,
    /// Host load average (1, 5, 15 min) and core count when the run started. Other work on the
    /// machine shows up as spread, so it belongs next to the numbers.
    #[serde(default)]
    pub load_start: String,
    /// The same when the run ended.
    #[serde(default)]
    pub load_end: String,
}

impl RunInfo {
    /// Collect everything, opening the device (by `serial`, else the only one) briefly to read
    /// its identity and link speed. A failure to reach the device leaves those fields as
    /// `unknown`; the benchmark itself reports the real error when it connects.
    pub async fn collect(serial: Option<&str>) -> Self {
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
            mtp_serial: None,
            load_start: load_average(),
            load_end: String::new(),
        };
        match probe_device(serial).await {
            Ok((device, firmware, speed, mtp_serial)) => {
                info.device = device;
                info.device_firmware = firmware;
                info.usb_speed = speed;
                info.mtp_serial = Some(mtp_serial);
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
                "Device:     {} (MTP device version {})",
                self.device, self.device_firmware
            ),
            format!("USB link:   {}", self.usb_speed),
            format!(
                "Host load:  {} at start, {} at end",
                self.load_start,
                if self.load_end.is_empty() {
                    "?"
                } else {
                    &self.load_end
                }
            ),
        ]
    }
}

/// Open the device the benchmark will use and match it to its listing entry, which is where the
/// link speed lives.
async fn probe_device(
    serial: Option<&str>,
) -> Result<(String, String, String, String), Box<dyn std::error::Error>> {
    let listed = MtpDevice::list_devices()?;
    let device = crate::mtp_rs_backend::open_device(serial).await?;
    let di = device.device_info();
    let name = format!("{} {}", di.manufacturer, di.model);
    let firmware = di.device_version.clone();
    let entry = listed
        .iter()
        .find(|d| d.serial_number.is_some() && d.serial_number.as_deref() == serial)
        .or_else(|| (listed.len() == 1).then(|| &listed[0]));
    let speed = match entry {
        Some(d) => usb_speed_label(d.speed),
        None => "unknown (couldn't match the open device to a USB listing)".to_string(),
    };
    let mtp_serial = di.serial_number.clone();
    device.close().await?;
    Ok((name, firmware, speed, mtp_serial))
}

/// The 1, 5, and 15-minute load averages and the core count, for example `8.5 5.0 10.2 (16 cores)`.
pub fn load_average() -> String {
    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
    let load = command_output("sysctl", &["-n", "vm.loadavg"])
        .map(|s| {
            s.trim_matches(|c| c == '{' || c == '}' || c == ' ')
                .to_string()
        })
        .or_else(|| {
            std::fs::read_to_string("/proc/loadavg")
                .ok()
                .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        })
        .unwrap_or_else(|| "unknown".to_string());
    format!("{load} ({cores} cores)")
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
///
/// It's the last commit that touched code (the library, the bench's sources, or the lockfile),
/// so committing saved results or docs between chunks of one run doesn't change it.
fn git_rev() -> String {
    let dir = env!("CARGO_MANIFEST_DIR");
    let root = format!("{dir}/../..");
    let code = [
        "crates/mtp-rs",
        "benchmarks/mtp-rs-vs-libmtp/src",
        "benchmarks/mtp-rs-vs-libmtp/Cargo.toml",
        "Cargo.toml",
        "Cargo.lock",
    ];
    let mut log = vec![
        "-C",
        root.as_str(),
        "log",
        "-1",
        "--format=%h",
        "--abbrev=12",
        "--",
    ];
    log.extend(code);
    let Some(rev) = command_output("git", &log) else {
        return "unknown".to_string();
    };
    let mut status = vec![
        "-C",
        root.as_str(),
        "status",
        "--porcelain",
        "--untracked-files=no",
        "--",
    ];
    status.extend(code);
    let dirty = Command::new("git")
        .args(&status)
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
            device_firmware: "1.0".into(),
            usb_speed: "USB 3.2 Gen 1 (5 Gbit/s)".into(),
            mtp_serial: None,
            load_start: "2.1 3.0 4.0 (16 cores)".into(),
            load_end: "5.5 4.0 4.0 (16 cores)".into(),
        };
        let all = info.lines().join("\n");
        for needle in [
            "2026-11-11",
            "macOS 27.0.1",
            "0.38.0 (abc123)",
            "1.1.23",
            "Pixel 9 Pro XL (MTP device version 1.0)",
            "5 Gbit/s",
            "2.1 3.0 4.0 (16 cores) at start, 5.5 4.0 4.0 (16 cores) at end",
        ] {
            assert!(all.contains(needle), "missing {needle} in\n{all}");
        }
    }
}
