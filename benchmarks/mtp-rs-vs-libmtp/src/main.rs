use clap::Parser;
use mtp_bench::report;
use mtp_bench::run_info::RunInfo;
use mtp_bench::runner::run_benchmarks;
use mtp_bench::types::{
    format_bytes, parse_sizes, BackendName, BenchmarkConfig, Operation, Pacing,
    DEFAULT_LIST_DURING_SIZES, DEFAULT_SIZES,
};
use std::time::Duration;

/// Benchmark mtp-rs vs libmtp for MTP file transfers.
#[derive(Parser, Debug)]
#[command(
    name = "mtp-bench",
    about = "Benchmark mtp-rs vs libmtp for MTP file transfers"
)]
struct Cli {
    /// Number of measured runs per scenario.
    #[arg(long, default_value_t = 5)]
    runs: usize,

    /// Number of warmup runs (not included in results).
    #[arg(long, default_value_t = 1)]
    warmup: usize,

    /// Comma-separated file sizes: 1MB,10MB,100MB,1GB.
    #[arg(long, default_value = DEFAULT_SIZES)]
    sizes: String,

    /// Comma-separated file sizes for `list_during`, which needs a file many times one ~2 s
    /// window to tell the pacings apart.
    #[arg(long, default_value = DEFAULT_LIST_DURING_SIZES)]
    list_during_sizes: String,

    /// Comma-separated operations: download,upload,list, plus the mtp-rs-only `stream`
    /// (held-open), `paced` (the default adaptive pacing), `windowed` (fixed windows, one run
    /// per `--windows` size), and `list_during` (a listing halfway through a download, held open,
    /// default pacing, and each `--windows` size).
    #[arg(long, default_value = "download,upload,list")]
    operations: String,

    /// Comma-separated window sizes in MiB for `windowed` and `list_during`.
    #[arg(long, default_value = "8")]
    windows: String,

    /// Comma-separated backends: mtp-rs,libmtp.
    #[arg(long, default_value = "mtp-rs,libmtp")]
    backends: String,

    /// Output format: table, markdown, csv, json.
    #[arg(long, default_value = "table")]
    output: String,

    /// The device to benchmark, by USB serial (`mtp-rs devices` lists them). Needed when more
    /// than one MTP device is connected; both backends refuse to guess.
    #[arg(long)]
    serial: Option<String>,

    /// Comma-separated JSON reports (from `--output json`) to merge and print in `--output`
    /// format, opening no device. For a long run split into chunks.
    #[arg(long)]
    render: Option<String>,
}

/// `windowed` expands to one operation per entry in `windows_mib`, and `list_during` to held
/// open, the default pacing, and one per entry in `windows_mib`.
fn parse_operations(input: &str, windows_mib: &[u32]) -> Result<Vec<Operation>, String> {
    let mut ops = Vec::new();
    for s in input.split(',') {
        match s.trim().to_lowercase().as_str() {
            "download" => ops.push(Operation::Download),
            "stream" | "download_stream" => ops.push(Operation::DownloadStream),
            "paced" | "download_paced" => ops.push(Operation::DownloadPaced),
            "windowed" | "download_windowed" => ops.extend(
                windows_mib
                    .iter()
                    .map(|&window_mib| Operation::DownloadWindowed { window_mib }),
            ),
            "upload" => ops.push(Operation::Upload),
            "list" | "list_files" | "listfiles" => ops.push(Operation::ListFiles),
            "list_during" | "list-during" => {
                let pacings = [Pacing::HeldOpen, Pacing::Default].into_iter().chain(
                    windows_mib
                        .iter()
                        .map(|&window_mib| Pacing::FixedWindow { window_mib }),
                );
                ops.extend(pacings.map(|pacing| Operation::ListDuringDownload { pacing }));
            }
            // One pacing at a time, so a long run can be split into chunks.
            "list_during_held" => ops.push(Operation::ListDuringDownload {
                pacing: Pacing::HeldOpen,
            }),
            "list_during_paced" => ops.push(Operation::ListDuringDownload {
                pacing: Pacing::Default,
            }),
            "list_during_windowed" => {
                ops.extend(
                    windows_mib
                        .iter()
                        .map(|&window_mib| Operation::ListDuringDownload {
                            pacing: Pacing::FixedWindow { window_mib },
                        }),
                )
            }
            other => {
                return Err(format!(
                    "Unknown operation '{}'. Expected: download, stream, paced, windowed, \
                     list_during, upload, list",
                    other
                ))
            }
        }
    }
    Ok(ops)
}

fn parse_windows(input: &str) -> Result<Vec<u32>, String> {
    input
        .split(',')
        .map(|s| match s.trim().parse::<u32>() {
            Ok(n) if (1..=4095).contains(&n) => Ok(n),
            _ => Err(format!("Invalid window '{s}': expected 1–4095 (MiB)")),
        })
        .collect()
}

fn parse_backends(input: &str) -> Result<Vec<BackendName>, String> {
    input
        .split(',')
        .map(|s| match s.trim().to_lowercase().as_str() {
            "mtp-rs" | "mtprs" => Ok(BackendName::MtpRs),
            "libmtp" => Ok(BackendName::Libmtp),
            other => Err(format!(
                "Unknown backend '{}'. Expected: mtp-rs, libmtp",
                other
            )),
        })
        .collect()
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Some(paths) = &cli.render {
        let mut reports = Vec::new();
        for path in paths.split(',').map(str::trim) {
            let parsed = std::fs::read_to_string(path)
                .map_err(|e| e.to_string())
                .and_then(|s| {
                    serde_json::from_str::<report::JsonReport>(&s).map_err(|e| e.to_string())
                });
            match parsed {
                Ok(r) => reports.push(r),
                Err(e) => {
                    eprintln!("Couldn't read report {path}: {e}");
                    std::process::exit(1);
                }
            }
        }
        match report::merge_reports(reports) {
            Ok((info, results)) => print_report(&cli.output, &info, &results),
            Err(e) => {
                eprintln!("Can't merge: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let file_sizes = match parse_sizes(&cli.sizes) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error parsing --sizes: {}", e);
            std::process::exit(1);
        }
    };

    let list_during_sizes = match parse_sizes(&cli.list_during_sizes) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error parsing --list-during-sizes: {}", e);
            std::process::exit(1);
        }
    };

    let windows = match parse_windows(&cli.windows) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Error parsing --windows: {}", e);
            std::process::exit(1);
        }
    };

    let operations = match parse_operations(&cli.operations, &windows) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Error parsing --operations: {}", e);
            std::process::exit(1);
        }
    };

    let backends = match parse_backends(&cli.backends) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error parsing --backends: {}", e);
            std::process::exit(1);
        }
    };

    let mut config = BenchmarkConfig {
        num_runs: cli.runs,
        warmup_runs: cli.warmup,
        file_sizes,
        list_during_sizes,
        operations,
        backends,
        serial: cli.serial.clone(),
        mtp_serial: None,
    };

    // Print configuration summary to stderr (keeps stdout clean for machine-readable output).
    eprintln!("=== mtp-bench ===");
    eprintln!("Runs:       {}", config.num_runs);
    eprintln!("Warmup:     {}", config.warmup_runs);
    eprintln!(
        "Sizes:      {}",
        config
            .file_sizes
            .iter()
            .map(|&s| format_bytes(s))
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!(
        "Operations: {}",
        config
            .operations
            .iter()
            .map(|o| o.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!(
        "Backends:   {}",
        config
            .backends
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if config
        .operations
        .iter()
        .any(|o| matches!(o, Operation::ListDuringDownload { .. }))
    {
        eprintln!(
            "List-during sizes: {}",
            config
                .list_during_sizes
                .iter()
                .map(|&s| format_bytes(s))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    eprintln!("Output:     {}", cli.output);
    eprintln!();

    let mut info = RunInfo::collect(config.serial.as_deref()).await;
    config.mtp_serial = info.mtp_serial.clone();
    for line in info.lines() {
        eprintln!("{line}");
    }
    // The probe just closed a session; give the device the same settle as between scenarios.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let results = match run_benchmarks(&config).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Benchmark failed: {}", e);
            std::process::exit(1);
        }
    };
    info.load_end = mtp_bench::run_info::load_average();

    if results.is_empty() {
        eprintln!("No results collected.");
        return;
    }

    eprintln!("\n=== Results ===\n");
    print_report(&cli.output, &info, &results);
}

/// Print the results in `output` format, exiting on an unknown format.
fn print_report(output: &str, info: &RunInfo, results: &[mtp_bench::types::BenchmarkResult]) {
    match output.to_lowercase().as_str() {
        "table" => {
            println!("{}", report::render_terminal_table(info, results));
        }
        "markdown" | "md" => {
            println!("{}", report::render_markdown_table(info, results));
        }
        "csv" => {
            println!("{}", report::render_csv(info, results));
        }
        "json" => match report::render_json(info, results) {
            Ok(json) => println!("{}", json),
            Err(e) => {
                eprintln!("Failed to serialize results to JSON: {}", e);
                std::process::exit(1);
            }
        },
        other => {
            eprintln!(
                "Unknown output format '{}'. Expected: table, markdown, csv, json",
                other
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_during_expands_to_held_default_and_each_window() {
        let ops = parse_operations("list_during", &[8, 64]).unwrap();
        let pacings: Vec<Pacing> = ops
            .iter()
            .map(|o| match o {
                Operation::ListDuringDownload { pacing } => *pacing,
                other => panic!("unexpected {other}"),
            })
            .collect();
        assert_eq!(
            pacings,
            vec![
                Pacing::HeldOpen,
                Pacing::Default,
                Pacing::FixedWindow { window_mib: 8 },
                Pacing::FixedWindow { window_mib: 64 },
            ]
        );
        assert_eq!(parse_operations("list-during", &[8]).unwrap().len(), 3);
        // The single-pacing names add up to the same set.
        let split = parse_operations(
            "list_during_held,list_during_paced,list_during_windowed",
            &[8, 64],
        )
        .unwrap();
        assert_eq!(split, ops);
    }

    #[test]
    fn other_operations_still_parse() {
        let ops = parse_operations("download, list,windowed", &[8, 32]).unwrap();
        assert_eq!(ops.len(), 4);
        assert!(parse_operations("nope", &[8]).is_err());
    }

    #[test]
    fn cli_defaults_parse() {
        let cli = Cli::parse_from(["mtp-bench"]);
        assert_eq!(parse_sizes(&cli.sizes).unwrap().len(), 4);
        assert_eq!(
            parse_sizes(&cli.list_during_sizes).unwrap(),
            vec![4_000_000_000]
        );
        assert_eq!(parse_windows(&cli.windows).unwrap(), vec![8]);
        assert_eq!(cli.serial, None);
        let cli = Cli::parse_from(["mtp-bench", "--serial", "46061FDAS000A4"]);
        assert_eq!(cli.serial.as_deref(), Some("46061FDAS000A4"));
    }
}
