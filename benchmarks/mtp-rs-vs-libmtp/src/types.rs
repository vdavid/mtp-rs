use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// Which MTP backend was used for a benchmark run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BackendName {
    /// Pure-Rust mtp-rs crate.
    MtpRs,
    /// C-library libmtp via libmtp-rs bindings.
    Libmtp,
}

impl fmt::Display for BackendName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendName::MtpRs => write!(f, "mtp-rs"),
            BackendName::Libmtp => write!(f, "libmtp"),
        }
    }
}

/// The kind of MTP operation being benchmarked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Operation {
    /// Download a file from the device to the host.
    Download,
    /// mtp-rs only: `DownloadPacing::HeldOpen`, chunks discarded (no buffering).
    DownloadStream,
    /// mtp-rs only: the default `DownloadPacing` (adaptive windows of ~2 s), chunks discarded.
    DownloadPaced,
    /// mtp-rs only: `DownloadPacing::FixedWindow` with this window size, chunks discarded.
    DownloadWindowed {
        /// Window size in MiB.
        window_mib: u32,
    },
    /// Upload a file from the host to the device.
    Upload,
    /// List all objects in a storage folder.
    ListFiles,
    /// mtp-rs only: download a big file with this pacing and, once half of it has arrived, list
    /// the storage root on the same session. Times both: the download from start to end (the cost
    /// of pacing), and the listing from issue to answer (the benefit).
    ListDuringDownload {
        /// How the download is paced.
        pacing: Pacing,
    },
}

impl Operation {
    /// Whether only the mtp-rs backend can run this operation.
    pub fn is_mtp_rs_only(self) -> bool {
        matches!(
            self,
            Operation::DownloadStream
                | Operation::DownloadPaced
                | Operation::DownloadWindowed { .. }
                | Operation::ListDuringDownload { .. }
        )
    }
}

/// The download pacing a [`Operation::ListDuringDownload`] run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Pacing {
    /// `DownloadPacing::HeldOpen`: one transfer, the listing waits for the whole rest of the file.
    HeldOpen,
    /// `DownloadPacing::default()`: windows of about 2 s.
    Default,
    /// `DownloadPacing::FixedWindow` with this window size.
    FixedWindow {
        /// Window size in MiB.
        window_mib: u32,
    },
}

impl Pacing {
    /// The library value this stands for.
    pub fn to_download_pacing(self) -> mtp_rs::DownloadPacing {
        match self {
            Pacing::HeldOpen => mtp_rs::DownloadPacing::HeldOpen,
            Pacing::Default => mtp_rs::DownloadPacing::default(),
            Pacing::FixedWindow { window_mib } => {
                mtp_rs::DownloadPacing::FixedWindow(window_mib * 1024 * 1024)
            }
        }
    }
}

impl fmt::Display for Pacing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pacing::HeldOpen => write!(f, "held open"),
            Pacing::Default => write!(f, "default (~2 s windows)"),
            Pacing::FixedWindow { window_mib } => write!(f, "fixed {window_mib} MiB windows"),
        }
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operation::Download => write!(f, "download"),
            Operation::DownloadStream => write!(f, "download_stream"),
            Operation::DownloadPaced => write!(f, "download_paced"),
            Operation::DownloadWindowed { window_mib } => {
                write!(f, "download_windowed_{window_mib:03}MiB")
            }
            Operation::Upload => write!(f, "upload"),
            Operation::ListFiles => write!(f, "list_files"),
            Operation::ListDuringDownload { pacing } => match pacing {
                Pacing::HeldOpen => write!(f, "list_during_held"),
                Pacing::Default => write!(f, "list_during_paced"),
                Pacing::FixedWindow { window_mib } => {
                    write!(f, "list_during_windowed_{window_mib:03}MiB")
                }
            },
        }
    }
}

/// Median of a set of durations (the mean of the middle two for an even count).
pub fn median(durations: &[Duration]) -> Duration {
    if durations.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = durations.to_vec();
    sorted.sort();
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2
    } else {
        sorted[mid]
    }
}

/// Results of a single benchmark scenario (one backend, one operation, one file size, multiple runs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    /// Which backend produced this result.
    pub backend: BackendName,
    /// The operation that was benchmarked.
    pub operation: Operation,
    /// File size in bytes (0 for list operations).
    pub file_size_bytes: u64,
    /// Wall-clock durations for each individual run (excluding warmup). For
    /// [`Operation::ListDuringDownload`], the whole download, listing included.
    pub durations: Vec<Duration>,
    /// [`Operation::ListDuringDownload`] only: per run, how long the listing issued halfway
    /// through the download took to answer. Same order as `durations`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub list_latencies: Vec<Duration>,
}

impl BenchmarkResult {
    /// A result with no listing latencies (every operation but `ListDuringDownload`).
    pub fn new(
        backend: BackendName,
        operation: Operation,
        file_size_bytes: u64,
        durations: Vec<Duration>,
    ) -> Self {
        Self {
            backend,
            operation,
            file_size_bytes,
            durations,
            list_latencies: Vec::new(),
        }
    }

    /// Median duration across all runs.
    pub fn median_duration(&self) -> Duration {
        median(&self.durations)
    }

    /// Median latency of the mid-download listing, `None` when this result has none.
    pub fn median_list_latency(&self) -> Option<Duration> {
        (!self.list_latencies.is_empty()).then(|| median(&self.list_latencies))
    }

    /// Worst latency of the mid-download listing, `None` when this result has none.
    pub fn max_list_latency(&self) -> Option<Duration> {
        self.list_latencies.iter().max().copied()
    }

    /// Mean duration across all runs.
    pub fn mean_duration(&self) -> Duration {
        if self.durations.is_empty() {
            return Duration::ZERO;
        }
        let sum: f64 = self.durations.iter().map(|d| d.as_secs_f64()).sum();
        Duration::from_secs_f64(sum / self.durations.len() as f64)
    }

    /// Standard deviation of durations (population std dev).
    pub fn std_dev(&self) -> Duration {
        if self.durations.len() < 2 {
            return Duration::ZERO;
        }
        let mean = self.mean_duration().as_secs_f64();
        let variance: f64 = self
            .durations
            .iter()
            .map(|d| {
                let diff = d.as_secs_f64() - mean;
                diff * diff
            })
            .sum::<f64>()
            / self.durations.len() as f64;
        Duration::from_secs_f64(variance.sqrt())
    }

    /// Throughput in bytes per second based on the median duration.
    /// Returns `None` for list operations (file_size_bytes == 0) or if median is zero.
    pub fn throughput_bytes_per_sec(&self) -> Option<f64> {
        if self.file_size_bytes == 0 {
            return None;
        }
        let median_secs = self.median_duration().as_secs_f64();
        if median_secs == 0.0 {
            return None;
        }
        Some(self.file_size_bytes as f64 / median_secs)
    }
}

/// Default `--sizes`. The 1 GB top size is there for pacing: at the ~268 MB/s a Pixel 9 Pro XL
/// reaches on USB 3, 100 MB fits in the first 8 MiB window plus one more, while 1 GB takes the
/// default pacing about three windows of ~500 MiB, so its cost shows.
pub const DEFAULT_SIZES: &str = "1MB,10MB,100MB,1GB";

/// Default `--list-during-sizes`. The listing is issued halfway, so what it waits for held open
/// is the rest of the file: 2 GB, about 7.5 s at 268 MB/s, against at most one ~2 s window with
/// the default pacing. A smaller file makes the two look alike on USB 3 (1 GB leaves ~1.9 s,
/// one window's worth). 4 GB stays under 4 GiB, so the run doesn't also cross into the
/// over-4-GiB size path.
pub const DEFAULT_LIST_DURING_SIZES: &str = "4GB";

/// Parse a comma-separated size list like `1MB,10MB,1GB` (decimal units: KB, MB, GB).
pub fn parse_sizes(input: &str) -> Result<Vec<u64>, String> {
    input
        .split(',')
        .map(|s| {
            let s = s.trim().to_uppercase();
            let (digits, unit) = if let Some(d) = s.strip_suffix("GB") {
                (d, 1_000_000_000)
            } else if let Some(d) = s.strip_suffix("MB") {
                (d, 1_000_000)
            } else if let Some(d) = s.strip_suffix("KB") {
                (d, 1_000)
            } else {
                (s.as_str(), 1)
            };
            digits
                .parse::<u64>()
                .map(|n| n * unit)
                .map_err(|e| format!("Invalid size '{}': {}", s, e))
        })
        .collect()
}

/// Configuration for a benchmark session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    /// Number of measured runs per scenario (after warmup).
    pub num_runs: usize,
    /// Number of warmup runs (not included in results).
    pub warmup_runs: usize,
    /// File sizes (in bytes) to test for upload/download operations.
    pub file_sizes: Vec<u64>,
    /// File sizes (in bytes) for [`Operation::ListDuringDownload`], which needs a file much
    /// bigger than one ~2 s window to show anything.
    pub list_during_sizes: Vec<u64>,
    /// Which operations to benchmark.
    pub operations: Vec<Operation>,
    /// Which backends to benchmark.
    pub backends: Vec<BackendName>,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            num_runs: 5,
            warmup_runs: 1,
            file_sizes: parse_sizes(DEFAULT_SIZES).expect("default sizes parse"),
            list_during_sizes: parse_sizes(DEFAULT_LIST_DURING_SIZES).expect("default sizes parse"),
            operations: vec![Operation::Download, Operation::Upload, Operation::ListFiles],
            backends: vec![BackendName::MtpRs, BackendName::Libmtp],
        }
    }
}

/// Format a byte count as a human-readable string (e.g., "10.0 MB").
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// Format a throughput value (bytes/sec) as a human-readable string.
pub fn format_throughput(bytes_per_sec: f64) -> String {
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;

    if bytes_per_sec >= GB {
        format!("{:.2} GB/s", bytes_per_sec / GB)
    } else if bytes_per_sec >= MB {
        format!("{:.2} MB/s", bytes_per_sec / MB)
    } else {
        format!("{:.2} KB/s", bytes_per_sec / 1_000.0)
    }
}

/// Format a `Duration` in a human-friendly way.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 60.0 {
        let mins = (secs / 60.0).floor() as u64;
        let remainder = secs - (mins as f64 * 60.0);
        format!("{}m {:.2}s", mins, remainder)
    } else if secs >= 1.0 {
        format!("{:.3}s", secs)
    } else {
        format!("{:.1}ms", secs * 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_median_odd() {
        let r = BenchmarkResult::new(
            BackendName::MtpRs,
            Operation::Download,
            1_000_000,
            vec![
                Duration::from_millis(300),
                Duration::from_millis(100),
                Duration::from_millis(200),
            ],
        );
        assert_eq!(r.median_duration(), Duration::from_millis(200));
    }

    #[test]
    fn test_median_even() {
        let r = BenchmarkResult::new(
            BackendName::MtpRs,
            Operation::Download,
            1_000_000,
            vec![
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(300),
                Duration::from_millis(400),
            ],
        );
        assert_eq!(r.median_duration(), Duration::from_millis(250));
    }

    #[test]
    fn test_throughput() {
        let r = BenchmarkResult::new(
            BackendName::MtpRs,
            Operation::Download,
            10_000_000,
            vec![Duration::from_secs(1)],
        );
        let tp = r.throughput_bytes_per_sec().unwrap();
        assert!((tp - 10_000_000.0).abs() < 0.01);
    }

    #[test]
    fn test_throughput_none_for_list() {
        let r = BenchmarkResult::new(
            BackendName::MtpRs,
            Operation::ListFiles,
            0,
            vec![Duration::from_secs(1)],
        );
        assert!(r.throughput_bytes_per_sec().is_none());
    }

    #[test]
    fn list_latency_stats_come_from_the_listing_not_the_download() {
        let mut r = BenchmarkResult::new(
            BackendName::MtpRs,
            Operation::ListDuringDownload {
                pacing: Pacing::Default,
            },
            4_000_000_000,
            vec![Duration::from_secs(15); 3],
        );
        assert_eq!(r.median_list_latency(), None);
        assert_eq!(r.max_list_latency(), None);
        r.list_latencies = vec![
            Duration::from_millis(900),
            Duration::from_millis(100),
            Duration::from_millis(400),
        ];
        assert_eq!(r.median_list_latency(), Some(Duration::from_millis(400)));
        assert_eq!(r.max_list_latency(), Some(Duration::from_millis(900)));
        assert_eq!(r.median_duration(), Duration::from_secs(15));
    }

    #[test]
    fn list_latencies_round_trip_and_stay_out_of_other_results_json() {
        let plain = BenchmarkResult::new(
            BackendName::MtpRs,
            Operation::ListFiles,
            0,
            vec![Duration::from_millis(5)],
        );
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("list_latencies"), "{json}");
        // Results saved before the field existed still load.
        let back: BenchmarkResult = serde_json::from_str(&json).unwrap();
        assert!(back.list_latencies.is_empty());
    }

    #[test]
    fn list_during_operations_are_mtp_rs_only_and_named_by_pacing() {
        let held = Operation::ListDuringDownload {
            pacing: Pacing::HeldOpen,
        };
        let windowed = Operation::ListDuringDownload {
            pacing: Pacing::FixedWindow { window_mib: 8 },
        };
        assert!(held.is_mtp_rs_only());
        assert_eq!(held.to_string(), "list_during_held");
        assert_eq!(windowed.to_string(), "list_during_windowed_008MiB");
        assert_eq!(
            Pacing::FixedWindow { window_mib: 8 }.to_download_pacing(),
            mtp_rs::DownloadPacing::FixedWindow(8 * 1024 * 1024)
        );
        assert_eq!(
            Pacing::Default.to_download_pacing(),
            mtp_rs::DownloadPacing::default()
        );
    }

    #[test]
    fn default_sizes_parse_and_reach_pacing_territory() {
        let sizes = parse_sizes(DEFAULT_SIZES).unwrap();
        assert_eq!(
            sizes,
            vec![1_000_000, 10_000_000, 100_000_000, 1_000_000_000]
        );
        let big = parse_sizes(DEFAULT_LIST_DURING_SIZES).unwrap();
        assert_eq!(big, vec![4_000_000_000]);
        // Under 4 GiB, so the run stays off the >4 GiB ContainerLength sentinel path.
        assert!(big.iter().all(|&s| s < 4 * 1024 * 1024 * 1024));
    }

    #[test]
    fn parse_sizes_units_and_errors() {
        assert_eq!(
            parse_sizes("500, 2kb,3MB,1GB").unwrap(),
            vec![500, 2_000, 3_000_000, 1_000_000_000]
        );
        assert!(parse_sizes("12XB").is_err());
        assert!(parse_sizes("").is_err());
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(10_000_000), "10.0 MB");
        assert_eq!(format_bytes(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(Duration::from_millis(50)), "50.0ms");
        assert_eq!(format_duration(Duration::from_secs_f64(1.234)), "1.234s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30.00s");
    }
}
