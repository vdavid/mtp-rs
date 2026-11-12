//! What a reader needs to trust a set of numbers: which code, which device, which link, when.

use crate::types::BackendName;
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
    /// The host machine: CPU, plus the model identifier on macOS, for example
    /// `Apple M1 Max (MacBookPro18,2)`. Throughput on USB 3 depends on the host's controller and
    /// CPU, so results from two hosts aren't one table. Empty in reports saved before it existed.
    #[serde(default)]
    pub host: String,
    /// mtp-rs crate version under test.
    pub mtp_rs_version: String,
    /// Git commit of the checkout, with `-dirty` when tracked files had local changes.
    pub git_rev: String,
    /// The system libmtp the baseline linked against (`pkg-config --modversion libmtp`), or why
    /// this build has none.
    pub libmtp_version: String,
    /// The backends this build has, for example `mtp-rs, libmtp`, or `mtp-rs (no libmtp: libmtp
    /// isn't built on Windows)`. Empty in reports saved before it existed.
    #[serde(default)]
    pub backends: String,
    /// How mtp-rs uploads go out on this device: in windows (Android's edit operations, over
    /// USB) or in one piece (WPD, cameras). The upload rows aren't the same operation across the
    /// two. Empty in reports saved before it existed.
    #[serde(default)]
    pub mtp_rs_uploads: String,
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
    /// machine shows up as spread, so it belongs next to the numbers. Windows has no load
    /// average, so there it's the CPU's busy share over one second (see [`host_load`]).
    #[serde(default)]
    pub load_start: String,
    /// The same when the run ended. On Windows, also the busy share over the whole run.
    #[serde(default)]
    pub load_end: String,
    /// The CPU counters when the run started, for the whole-run share on Windows.
    #[serde(skip)]
    pub(crate) cpu_at_start: Option<cpu::Times>,
}

impl RunInfo {
    /// Collect everything, opening the device (by `serial`, else the only one) briefly to read
    /// its identity and link speed. A failure to reach the device leaves those fields as
    /// `unknown`; the benchmark itself reports the real error when it connects.
    pub async fn collect(serial: Option<&str>) -> Self {
        let cpu_at_start = cpu::Times::now();
        let mut info = RunInfo {
            date: chrono::Local::now()
                .format("%Y-%m-%d %H:%M %:z")
                .to_string(),
            os: host_os(),
            host: host_machine(),
            mtp_rs_version: mtp_rs_version().to_string(),
            git_rev: git_rev(),
            libmtp_version: libmtp_version(),
            backends: backends_label(),
            mtp_rs_uploads: "unknown".to_string(),
            device: "unknown".to_string(),
            device_firmware: "unknown".to_string(),
            usb_speed: "unknown".to_string(),
            mtp_serial: None,
            load_start: host_load(None),
            load_end: String::new(),
            cpu_at_start,
        };
        match probe_device(serial).await {
            Ok(p) => {
                info.device = p.name;
                info.device_firmware = p.firmware;
                info.usb_speed = p.speed;
                info.mtp_serial = Some(p.mtp_serial);
                info.mtp_rs_uploads = p.uploads;
            }
            Err(e) => eprintln!("Couldn't read the device's identity: {e}"),
        }
        info
    }

    /// Record the host's load at the end of the run.
    pub fn finish(&mut self) {
        self.load_end = host_load(self.cpu_at_start);
    }

    /// One line per fact, for the terminal and Markdown reports.
    pub fn lines(&self) -> Vec<String> {
        let or_unknown = |s: &str| {
            if s.is_empty() {
                "unknown".to_string()
            } else {
                s.to_string()
            }
        };
        vec![
            format!("Date:       {}", self.date),
            format!("Host:       {}", self.host),
            format!("Host OS:    {}", self.os),
            format!("mtp-rs:     {} ({})", self.mtp_rs_version, self.git_rev),
            format!("libmtp:     {}", self.libmtp_version),
            format!("Backends:   {}", or_unknown(&self.backends)),
            format!(
                "Device:     {} (MTP device version {})",
                self.device, self.device_firmware
            ),
            format!("USB link:   {}", self.usb_speed),
            format!("Uploads:    mtp-rs {}", or_unknown(&self.mtp_rs_uploads)),
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
///
/// The speed comes from the USB listing on every OS, also on Windows, where the device itself is
/// opened through WPD: nusb reads it from the hub the phone hangs off, whatever driver the phone
/// is bound to.
async fn probe_device(serial: Option<&str>) -> Result<Probe, Box<dyn std::error::Error>> {
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
    let uploads = uploads_label(device.capabilities().supports_windowed_upload);
    let mtp_serial = di.serial_number.clone();
    device.close().await?;
    Ok(Probe {
        name,
        firmware,
        speed,
        mtp_serial,
        uploads,
    })
}

/// What [`probe_device`] read.
struct Probe {
    name: String,
    firmware: String,
    speed: String,
    mtp_serial: String,
    uploads: String,
}

/// How mtp-rs's default upload goes out, given whether the device has windowed uploads. The bench
/// sends no modification date, so the default pacing windows whenever it can.
pub fn uploads_label(windowed: bool) -> String {
    if windowed {
        "in windows (Android's edit operations)".to_string()
    } else {
        "in one piece (no windowed uploads on this device or backend)".to_string()
    }
}

fn libmtp_version() -> String {
    if crate::types::LIBMTP_BUILT_IN {
        command_output("pkg-config", &["--modversion", "libmtp"])
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        format!("not in this build ({})", crate::types::NO_LIBMTP_REASON)
    }
}

fn backends_label() -> String {
    let names: Vec<String> = BackendName::available()
        .iter()
        .map(|b| b.to_string())
        .collect();
    let mut label = names.join(", ");
    if !crate::types::LIBMTP_BUILT_IN {
        label.push_str(&format!(" (no libmtp: {})", crate::types::NO_LIBMTP_REASON));
    }
    label
}

/// The host's load, plus the core count.
///
/// - Unix: the 1, 5, and 15-minute load averages, for example `8.5 5.0 10.2 (16 cores)`.
/// - Windows, which has no load average: the share of CPU time spent busy over the next second,
///   from `GetSystemTimes`, and with `since`, also over the whole time since then, for example
///   `CPU 12% busy over 1 s, 31% over the run (4 cores)`. Busy is everything but the idle
///   thread, summed over all logical processors.
pub fn host_load(since: Option<cpu::Times>) -> String {
    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
    let load = if cfg!(windows) {
        cpu_busy(since)
    } else {
        let _ = since;
        command_output("sysctl", &["-n", "vm.loadavg"])
            .map(|s| {
                s.trim_matches(|c| c == '{' || c == '}' || c == ' ')
                    .to_string()
            })
            .or_else(|| {
                std::fs::read_to_string("/proc/loadavg")
                    .ok()
                    .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
            })
    }
    .unwrap_or_else(|| "unknown".to_string());
    format!("{load} ({cores} cores)")
}

/// `CPU 12% busy over 1 s`, plus `, 31% over the run` with a starting sample.
fn cpu_busy(since: Option<cpu::Times>) -> Option<String> {
    let a = cpu::Times::now()?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    let b = cpu::Times::now()?;
    let mut s = format!("CPU {:.0}% busy over 1 s", a.busy_share_until(&b)? * 100.0);
    if let Some(share) = since.and_then(|start| start.busy_share_until(&b)) {
        s.push_str(&format!(", {:.0}% over the run", share * 100.0));
    }
    Some(s)
}

/// System-wide CPU time counters. Only Windows has them here; elsewhere [`Times::now`] is `None`.
pub mod cpu {
    /// Idle and total CPU time since boot, in 100 ns units, summed over all logical processors.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Times {
        idle: u64,
        total: u64,
    }

    impl Times {
        /// The counters now.
        #[cfg(windows)]
        pub fn now() -> Option<Self> {
            #[repr(C)]
            #[derive(Default)]
            struct FileTime {
                low: u32,
                high: u32,
            }
            #[link(name = "kernel32")]
            extern "system" {
                fn GetSystemTimes(
                    idle: *mut FileTime,
                    kernel: *mut FileTime,
                    user: *mut FileTime,
                ) -> i32;
            }
            let mut idle = FileTime::default();
            let mut kernel = FileTime::default();
            let mut user = FileTime::default();
            // SAFETY: three valid, writable FILETIMEs, which is all the call takes.
            let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
            let v = |t: FileTime| (u64::from(t.high) << 32) | u64::from(t.low);
            // Kernel time includes idle time.
            (ok != 0).then(|| Self::from_parts(v(idle), v(kernel) + v(user)))
        }

        /// The counters now.
        #[cfg(not(windows))]
        pub fn now() -> Option<Self> {
            None
        }

        /// From idle and total time.
        pub fn from_parts(idle: u64, total: u64) -> Self {
            Self { idle, total }
        }

        /// The share of CPU time spent busy between `self` and `later`, 0–1. `None` when no time
        /// passed.
        pub fn busy_share_until(&self, later: &Times) -> Option<f64> {
            let total = later.total.checked_sub(self.total)?;
            let idle = later.idle.checked_sub(self.idle)?;
            (total > 0).then(|| total.saturating_sub(idle) as f64 / total as f64)
        }
    }
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
        "windows" => windows_version(),
        _ => command_output("uname", &["-sr"]),
    };
    detail.unwrap_or_else(|| std::env::consts::OS.to_string())
}

const WINDOWS_CURRENT_VERSION: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// For example `Windows 10 Pro 22H2 (build 19045.6456)`, from the registry. `cmd /C ver` would
/// be shorter, but it's localized (Hungarian on Mami) and comes in the OEM code page.
fn windows_version() -> Option<String> {
    let get = |name| reg_value(WINDOWS_CURRENT_VERSION, name);
    let build = get("CurrentBuild")?;
    let product = get("ProductName").unwrap_or_else(|| "Windows".to_string());
    let ubr = get("UBR").and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
    Some(windows_label(
        &product,
        get("DisplayVersion").as_deref(),
        &build,
        ubr,
    ))
}

/// Windows 11 still says "Windows 10" in `ProductName`; its builds start at 22000.
fn windows_label(product: &str, display: Option<&str>, build: &str, ubr: Option<u32>) -> String {
    let product = match build.parse::<u32>() {
        Ok(b) if b >= 22000 => product.replacen("Windows 10", "Windows 11", 1),
        _ => product.to_string(),
    };
    let mut s = product;
    if let Some(d) = display {
        s.push(' ');
        s.push_str(d);
    }
    match ubr {
        Some(u) => s.push_str(&format!(" (build {build}.{u})")),
        None => s.push_str(&format!(" (build {build})")),
    }
    s
}

/// One registry value through `reg query`, which every Windows has.
fn reg_value(key: &str, name: &str) -> Option<String> {
    let out = command_output("reg", &["query", key, "/v", name])?;
    parse_reg_value(&out, name)
}

/// `reg query` prints `    Name    REG_SZ    the value`; the value is everything after the type.
fn parse_reg_value(output: &str, name: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next()? != name || !parts.next()?.starts_with("REG_") {
            return None;
        }
        let value = parts.collect::<Vec<_>>().join(" ");
        (!value.is_empty()).then_some(value)
    })
}

fn host_machine() -> String {
    let cpu = match std::env::consts::OS {
        "macos" => command_output("sysctl", &["-n", "machdep.cpu.brand_string"]),
        "linux" => std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        }),
        "windows" => reg_value(
            r"HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0",
            "ProcessorNameString",
        )
        .or_else(|| std::env::var("PROCESSOR_IDENTIFIER").ok()),
        _ => None,
    }
    .unwrap_or_else(|| "unknown CPU".to_string());
    let model = match std::env::consts::OS {
        "macos" => command_output("sysctl", &["-n", "hw.model"]),
        // For example `Dell Inc. Latitude 7420`: the USB controller comes with the machine.
        "windows" => {
            let bios = r"HKLM\HARDWARE\DESCRIPTION\System\BIOS";
            let parts: Vec<String> = ["SystemManufacturer", "SystemProductName"]
                .iter()
                .filter_map(|n| reg_value(bios, n))
                .collect();
            (!parts.is_empty()).then(|| parts.join(" "))
        }
        _ => None,
    };
    match model {
        Some(model) => format!("{cpu} ({model})"),
        None => cpu,
    }
}

/// The paths whose last commit is "the code under test". Keep in sync with `build.rs`.
const CODE_PATHS: [&str; 5] = [
    "crates/mtp-rs",
    "benchmarks/mtp-rs-vs-libmtp/src",
    "benchmarks/mtp-rs-vs-libmtp/Cargo.toml",
    "Cargo.toml",
    "Cargo.lock",
];

/// The checkout's commit, from the bench crate's own directory so it works from any cwd.
///
/// It's the last commit that touched code (the library, the bench's sources, or the lockfile),
/// so committing saved results or docs between chunks of one run doesn't change it.
///
/// A binary copied to another machine has no checkout to ask, so there it's the commit
/// `build.rs` recorded at build time, marked `(at build time)`.
fn git_rev() -> String {
    checkout_git_rev().unwrap_or_else(|| format!("{} (at build time)", env!("MTP_BENCH_GIT_REV")))
}

fn checkout_git_rev() -> Option<String> {
    let dir = env!("CARGO_MANIFEST_DIR");
    if !std::path::Path::new(dir).exists() {
        return None;
    }
    let root = format!("{dir}/../..");
    let code = CODE_PATHS;
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
    let rev = command_output("git", &log)?;
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
    Some(if dirty { format!("{rev}-dirty") } else { rev })
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
            host: "Apple M1 Max (MacBookPro18,2)".into(),
            mtp_rs_version: "0.38.0".into(),
            git_rev: "abc123".into(),
            libmtp_version: "1.1.23".into(),
            device: "Google Pixel 9 Pro XL".into(),
            device_firmware: "1.0".into(),
            usb_speed: "USB 3.2 Gen 1 (5 Gbit/s)".into(),
            mtp_serial: None,
            load_start: "2.1 3.0 4.0 (16 cores)".into(),
            load_end: "5.5 4.0 4.0 (16 cores)".into(),
            backends: "mtp-rs, libmtp".into(),
            mtp_rs_uploads: uploads_label(true),
            cpu_at_start: None,
        };
        let all = info.lines().join("\n");
        for needle in [
            "2026-11-11",
            "macOS 27.0.1",
            "Apple M1 Max (MacBookPro18,2)",
            "0.38.0 (abc123)",
            "1.1.23",
            "Pixel 9 Pro XL (MTP device version 1.0)",
            "5 Gbit/s",
            "2.1 3.0 4.0 (16 cores) at start, 5.5 4.0 4.0 (16 cores) at end",
            "Backends:   mtp-rs, libmtp",
            "Uploads:    mtp-rs in windows",
        ] {
            assert!(all.contains(needle), "missing {needle} in\n{all}");
        }
    }

    #[test]
    fn the_report_says_which_backends_this_build_has() {
        let label = backends_label();
        assert!(label.starts_with("mtp-rs"), "{label}");
        assert_eq!(label.contains("libmtp:"), !crate::types::LIBMTP_BUILT_IN);
        if !crate::types::LIBMTP_BUILT_IN {
            assert!(libmtp_version().starts_with("not in this build"));
        }
    }

    #[test]
    fn reg_query_values_parse_and_windows_11_is_named() {
        let out = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\r\n    \
                   ProductName    REG_SZ    Windows 10 Pro\r\n\r\n";
        assert_eq!(
            parse_reg_value(out, "ProductName").as_deref(),
            Some("Windows 10 Pro")
        );
        assert_eq!(parse_reg_value(out, "UBR"), None);
        assert_eq!(
            windows_label("Windows 10 Pro", Some("22H2"), "19045", Some(6456)),
            "Windows 10 Pro 22H2 (build 19045.6456)"
        );
        assert_eq!(
            windows_label("Windows 10 Pro", Some("24H2"), "26100", None),
            "Windows 11 Pro 24H2 (build 26100)"
        );
    }

    #[test]
    fn cpu_busy_share_is_the_non_idle_part_of_the_elapsed_time() {
        let a = cpu::Times::from_parts(1_000, 4_000);
        let b = cpu::Times::from_parts(1_300, 5_000);
        assert_eq!(a.busy_share_until(&b), Some(0.7));
        assert_eq!(a.busy_share_until(&a), None);
        assert_eq!(b.busy_share_until(&a), None);
    }

    #[test]
    fn a_copied_binary_still_names_its_commit() {
        let rev = env!("MTP_BENCH_GIT_REV");
        assert!(!rev.is_empty());
        // In this checkout, the live answer is used.
        assert!(!git_rev().contains("(at build time)"));
    }
}
