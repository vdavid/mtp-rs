use clap::Parser;
use mtp_bench::report;
use mtp_bench::runner::run_benchmarks;
use mtp_bench::types::{format_bytes, BackendName, BenchmarkConfig, Operation};

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
    #[arg(long, default_value = "1MB,10MB,100MB")]
    sizes: String,

    /// Comma-separated operations: download,upload,list, plus the mtp-rs-only `stream`
    /// (held-open), `paced` (the default adaptive pacing), and `windowed` (fixed windows, one run
    /// per `--windows` size).
    #[arg(long, default_value = "download,upload,list")]
    operations: String,

    /// Comma-separated window sizes in MiB for the `windowed` operation.
    #[arg(long, default_value = "8")]
    windows: String,

    /// Comma-separated backends: mtp-rs,libmtp.
    #[arg(long, default_value = "mtp-rs,libmtp")]
    backends: String,

    /// Output format: table, markdown, csv, json.
    #[arg(long, default_value = "table")]
    output: String,
}

fn parse_sizes(input: &str) -> Result<Vec<u64>, String> {
    input
        .split(',')
        .map(|s| {
            let s = s.trim().to_uppercase();
            if s.ends_with("GB") {
                s.trim_end_matches("GB")
                    .parse::<u64>()
                    .map(|n| n * 1_000_000_000)
                    .map_err(|e| format!("Invalid size '{}': {}", s, e))
            } else if s.ends_with("MB") {
                s.trim_end_matches("MB")
                    .parse::<u64>()
                    .map(|n| n * 1_000_000)
                    .map_err(|e| format!("Invalid size '{}': {}", s, e))
            } else if s.ends_with("KB") {
                s.trim_end_matches("KB")
                    .parse::<u64>()
                    .map(|n| n * 1_000)
                    .map_err(|e| format!("Invalid size '{}': {}", s, e))
            } else {
                s.parse::<u64>()
                    .map_err(|e| format!("Invalid size '{}': {}", s, e))
            }
        })
        .collect()
}

/// `windowed` expands to one operation per entry in `windows_mib`.
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
            other => {
                return Err(format!(
                "Unknown operation '{}'. Expected: download, stream, paced, windowed, upload, list",
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

    let file_sizes = match parse_sizes(&cli.sizes) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error parsing --sizes: {}", e);
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

    let config = BenchmarkConfig {
        num_runs: cli.runs,
        warmup_runs: cli.warmup,
        file_sizes,
        operations,
        backends,
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
    eprintln!("Output:     {}", cli.output);
    eprintln!();

    let results = match run_benchmarks(&config).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Benchmark failed: {}", e);
            std::process::exit(1);
        }
    };

    if results.is_empty() {
        eprintln!("No results collected.");
        return;
    }

    eprintln!("\n=== Results ===\n");

    match cli.output.to_lowercase().as_str() {
        "table" => {
            println!("{}", report::render_terminal_table(&results));
        }
        "markdown" | "md" => {
            println!("{}", report::render_markdown_table(&results));
        }
        "csv" => {
            println!("{}", report::render_csv(&results));
        }
        "json" => match report::render_json(&results) {
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
