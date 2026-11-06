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
    /// mtp-rs only: held-open `Storage::download(Full)`, chunks discarded (no buffering).
    DownloadStream,
    /// mtp-rs only: `Storage::download_windowed` with this window size, windows discarded.
    DownloadWindowed {
        /// Window size in MiB.
        window_mib: u32,
    },
    /// Upload a file from the host to the device.
    Upload,
    /// List all objects in a storage folder.
    ListFiles,
}

impl Operation {
    /// Whether only the mtp-rs backend can run this operation.
    pub fn is_mtp_rs_only(self) -> bool {
        matches!(
            self,
            Operation::DownloadStream | Operation::DownloadWindowed { .. }
        )
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operation::Download => write!(f, "download"),
            Operation::DownloadStream => write!(f, "download_stream"),
            Operation::DownloadWindowed { window_mib } => {
                write!(f, "download_windowed_{window_mib:03}MiB")
            }
            Operation::Upload => write!(f, "upload"),
            Operation::ListFiles => write!(f, "list_files"),
        }
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
    /// Wall-clock durations for each individual run (excluding warmup).
    pub durations: Vec<Duration>,
}

impl BenchmarkResult {
    /// Median duration across all runs.
    #[allow(clippy::manual_is_multiple_of)]
    pub fn median_duration(&self) -> Duration {
        if self.durations.is_empty() {
            return Duration::ZERO;
        }
        let mut sorted: Vec<f64> = self.durations.iter().map(|d| d.as_secs_f64()).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mid = sorted.len() / 2;
        let median = if sorted.len() % 2 == 0 {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        };
        Duration::from_secs_f64(median)
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

/// Configuration for a benchmark session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    /// Number of measured runs per scenario (after warmup).
    pub num_runs: usize,
    /// Number of warmup runs (not included in results).
    pub warmup_runs: usize,
    /// File sizes (in bytes) to test for upload/download operations.
    pub file_sizes: Vec<u64>,
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
            file_sizes: vec![
                1_000_000,   // 1 MB
                10_000_000,  // 10 MB
                100_000_000, // 100 MB
            ],
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
        let r = BenchmarkResult {
            backend: BackendName::MtpRs,
            operation: Operation::Download,
            file_size_bytes: 1_000_000,
            durations: vec![
                Duration::from_millis(300),
                Duration::from_millis(100),
                Duration::from_millis(200),
            ],
        };
        assert_eq!(r.median_duration(), Duration::from_millis(200));
    }

    #[test]
    fn test_median_even() {
        let r = BenchmarkResult {
            backend: BackendName::MtpRs,
            operation: Operation::Download,
            file_size_bytes: 1_000_000,
            durations: vec![
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(300),
                Duration::from_millis(400),
            ],
        };
        assert_eq!(r.median_duration(), Duration::from_millis(250));
    }

    #[test]
    fn test_throughput() {
        let r = BenchmarkResult {
            backend: BackendName::MtpRs,
            operation: Operation::Download,
            file_size_bytes: 10_000_000,
            durations: vec![Duration::from_secs(1)],
        };
        let tp = r.throughput_bytes_per_sec().unwrap();
        assert!((tp - 10_000_000.0).abs() < 0.01);
    }

    #[test]
    fn test_throughput_none_for_list() {
        let r = BenchmarkResult {
            backend: BackendName::MtpRs,
            operation: Operation::ListFiles,
            file_size_bytes: 0,
            durations: vec![Duration::from_secs(1)],
        };
        assert!(r.throughput_bytes_per_sec().is_none());
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
