use crate::libmtp_backend::LibmtpBackend;
use crate::mtp_rs_backend::MtpRsBackend;
use crate::types::{format_bytes, BackendName, BenchmarkConfig, BenchmarkResult, Operation};
use mtp_rs::DownloadPacing;
use std::time::{Duration, Instant};
/// Run all benchmarks described by `config` and return collected results.
///
/// For each (operation, file_size) scenario, backends are benchmarked one at a time
/// (connect, run, cleanup) because both backends open the same USB device and cannot
/// coexist.
pub async fn run_benchmarks(
    config: &BenchmarkConfig,
) -> Result<Vec<BenchmarkResult>, Box<dyn std::error::Error>> {
    eprintln!("Connecting to MTP device...");
    let mut results = Vec::new();

    for &operation in &config.operations {
        let sizes: Vec<u64> = if operation == Operation::ListFiles {
            // ListFiles does not depend on file size; run once with size 0.
            vec![0]
        } else {
            config.file_sizes.clone()
        };

        for &file_size in &sizes {
            for &backend in &config.backends {
                if operation.is_mtp_rs_only() && backend != BackendName::MtpRs {
                    continue;
                }
                let size_label = if file_size == 0 {
                    "N/A".to_string()
                } else {
                    format_bytes(file_size)
                };
                eprintln!("\nBenchmarking {} {} {}...", backend, operation, size_label);

                let num_runs = config.num_runs;
                let warmup_runs = config.warmup_runs;

                let result = match backend {
                    BackendName::MtpRs => {
                        run_mtp_rs_scenario(operation, file_size, num_runs, warmup_runs).await?
                    }
                    BackendName::Libmtp => {
                        // Run the entire libmtp scenario on a blocking thread since
                        // libmtp-rs types are not Send and all calls are synchronous.
                        let r: Result<BenchmarkResult, Box<dyn std::error::Error + Send + Sync>> =
                            tokio::task::spawn_blocking(move || {
                                run_libmtp_scenario(operation, file_size, num_runs, warmup_runs)
                            })
                            .await?;
                        r.map_err(|e| -> Box<dyn std::error::Error> { e })?
                    }
                };

                results.push(result);

                // Brief pause after each scenario to let the USB device settle
                // before the next backend/size reconnects.
                eprintln!("  (waiting 2s for USB device to settle...)");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }

    Ok(results)
}

/// Run a single scenario with the mtp-rs (async) backend.
async fn run_mtp_rs_scenario(
    operation: Operation,
    file_size: u64,
    num_runs: usize,
    warmup_runs: usize,
) -> Result<BenchmarkResult, Box<dyn std::error::Error>> {
    let test_data = vec![0xABu8; file_size as usize];
    let total_iterations = warmup_runs + num_runs;
    let mut durations = Vec::with_capacity(num_runs);

    eprintln!("  Connecting (mtp-rs)...");
    let backend = MtpRsBackend::connect().await?;
    eprintln!("  Device: {}", backend.device_description());

    for i in 0..total_iterations {
        let is_warmup = i < warmup_runs;
        let label = if is_warmup {
            format!("  [warmup {}/{}]", i + 1, warmup_runs)
        } else {
            format!("  [run {}/{}]", i - warmup_runs + 1, num_runs)
        };

        let elapsed = match operation {
            Operation::Download => {
                // Setup: upload a test file (not timed).
                let handle = backend
                    .upload("bench-download-file.bin", &test_data)
                    .await?;
                let start = Instant::now();
                let _data = backend.download(handle).await?;
                let elapsed = start.elapsed();
                backend.delete(handle).await?;
                elapsed
            }
            Operation::DownloadStream
            | Operation::DownloadPaced
            | Operation::DownloadWindowed { .. } => {
                let handle = backend
                    .upload("bench-download-file.bin", &test_data)
                    .await?;
                let start = Instant::now();
                let bytes = match operation {
                    Operation::DownloadWindowed { window_mib } => {
                        let window = DownloadPacing::FixedWindow(window_mib * 1024 * 1024);
                        backend.download_paced(handle, window).await?
                    }
                    Operation::DownloadPaced => {
                        backend
                            .download_paced(handle, DownloadPacing::default())
                            .await?
                    }
                    _ => backend.download_stream(handle).await?,
                };
                let elapsed = start.elapsed();
                if bytes != file_size {
                    return Err(format!("read {bytes} bytes, expected {file_size}").into());
                }
                backend.delete(handle).await?;
                elapsed
            }
            Operation::Upload => {
                let filename = format!("bench-upload-{}.bin", i);
                let start = Instant::now();
                let handle = backend.upload(&filename, &test_data).await?;
                let elapsed = start.elapsed();
                backend.delete(handle).await?;
                elapsed
            }
            Operation::ListFiles => {
                let start = Instant::now();
                let _count = backend.list_objects().await?;
                start.elapsed()
            }
        };

        eprintln!("{} {:.3}s", label, elapsed.as_secs_f64());

        if !is_warmup {
            durations.push(elapsed);
        }
    }

    backend.cleanup().await?;

    Ok(BenchmarkResult {
        backend: BackendName::MtpRs,
        operation,
        file_size_bytes: file_size,
        durations,
    })
}

/// Run a single scenario with the libmtp (sync) backend.
/// This function is meant to be called from `spawn_blocking`.
fn run_libmtp_scenario(
    operation: Operation,
    file_size: u64,
    num_runs: usize,
    warmup_runs: usize,
) -> Result<BenchmarkResult, Box<dyn std::error::Error + Send + Sync>> {
    let test_data = vec![0xABu8; file_size as usize];
    let total_iterations = warmup_runs + num_runs;
    let mut durations = Vec::with_capacity(num_runs);

    eprintln!("  Connecting (libmtp)...");
    let backend = LibmtpBackend::connect().map_err(|e| format!("libmtp connect failed: {e}"))?;
    eprintln!("  Device: {}", backend.device_description());

    for i in 0..total_iterations {
        let is_warmup = i < warmup_runs;
        let label = if is_warmup {
            format!("  [warmup {}/{}]", i + 1, warmup_runs)
        } else {
            format!("  [run {}/{}]", i - warmup_runs + 1, num_runs)
        };

        let elapsed = match operation {
            Operation::DownloadStream
            | Operation::DownloadPaced
            | Operation::DownloadWindowed { .. } => {
                return Err(format!("{operation} is mtp-rs only").into());
            }
            Operation::Download => {
                // Setup: upload a test file (not timed).
                let file_id = backend
                    .upload("bench-download-file.bin", &test_data)
                    .map_err(|e| format!("libmtp upload (setup) failed: {e}"))?;
                let start = Instant::now();
                let _data = backend
                    .download(file_id)
                    .map_err(|e| format!("libmtp download failed: {e}"))?;
                let elapsed = start.elapsed();
                backend
                    .delete(file_id)
                    .map_err(|e| format!("libmtp delete (cleanup) failed: {e}"))?;
                elapsed
            }
            Operation::Upload => {
                let filename = format!("bench-upload-{}.bin", i);
                let start = Instant::now();
                let file_id = backend
                    .upload(&filename, &test_data)
                    .map_err(|e| format!("libmtp upload failed: {e}"))?;
                let elapsed = start.elapsed();
                backend
                    .delete(file_id)
                    .map_err(|e| format!("libmtp delete (cleanup) failed: {e}"))?;
                elapsed
            }
            Operation::ListFiles => {
                let start = Instant::now();
                let _count = backend
                    .list_objects()
                    .map_err(|e| format!("libmtp list_objects failed: {e}"))?;
                start.elapsed()
            }
        };

        eprintln!("{} {:.3}s", label, elapsed.as_secs_f64());

        if !is_warmup {
            durations.push(elapsed);
        }
    }

    backend
        .cleanup()
        .map_err(|e| format!("libmtp cleanup failed: {e}"))?;

    Ok(BenchmarkResult {
        backend: BackendName::Libmtp,
        operation,
        file_size_bytes: file_size,
        durations,
    })
}
