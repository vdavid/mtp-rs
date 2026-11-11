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
        let sizes: Vec<u64> = match operation {
            // ListFiles does not depend on file size; run once with size 0.
            Operation::ListFiles => vec![0],
            Operation::ListDuringDownload { .. } => config.list_during_sizes.clone(),
            _ => config.file_sizes.clone(),
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

                let serial = config.serial.clone();
                let mtp_serial = config.mtp_serial.clone();
                let result = match backend {
                    BackendName::MtpRs => {
                        run_mtp_rs_scenario(
                            serial.as_deref(),
                            operation,
                            file_size,
                            num_runs,
                            warmup_runs,
                        )
                        .await?
                    }
                    BackendName::Libmtp => {
                        // Run the entire libmtp scenario on a blocking thread since
                        // libmtp-rs types are not Send and all calls are synchronous.
                        let r: Result<BenchmarkResult, Box<dyn std::error::Error + Send + Sync>> =
                            tokio::task::spawn_blocking(move || {
                                run_libmtp_scenario(
                                    serial.as_deref(),
                                    mtp_serial.as_deref(),
                                    operation,
                                    file_size,
                                    num_runs,
                                    warmup_runs,
                                )
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
    serial: Option<&str>,
    operation: Operation,
    file_size: u64,
    num_runs: usize,
    warmup_runs: usize,
) -> Result<BenchmarkResult, Box<dyn std::error::Error>> {
    let total_iterations = warmup_runs + num_runs;
    let mut durations = Vec::with_capacity(num_runs);
    let mut list_latencies = Vec::new();

    eprintln!("  Connecting (mtp-rs)...");
    let backend = MtpRsBackend::connect(serial).await?;
    eprintln!("  Device: {}", backend.device_description());

    // The mid-download listing reads the same file every run (reads don't change it), so it's
    // uploaded once: at 4 GB, an upload per run would cost more than the runs themselves.
    let shared_file = if let Operation::ListDuringDownload { .. } = operation {
        eprintln!(
            "  Uploading the {} test file (untimed)...",
            format_bytes(file_size)
        );
        Some(backend.upload("bench-list-during.bin", file_size).await?)
    } else {
        None
    };

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
                let handle = backend.upload("bench-download-file.bin", file_size).await?;
                let start = Instant::now();
                let _data = backend.download(handle).await?;
                let elapsed = start.elapsed();
                backend.delete(handle).await?;
                elapsed
            }
            Operation::DownloadStream
            | Operation::DownloadPaced
            | Operation::DownloadWindowed { .. } => {
                let handle = backend.upload("bench-download-file.bin", file_size).await?;
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
                let handle = backend.upload(&filename, file_size).await?;
                let elapsed = start.elapsed();
                backend.delete(handle).await?;
                elapsed
            }
            Operation::ListFiles => {
                let start = Instant::now();
                let _count = backend.list_objects().await?;
                start.elapsed()
            }
            Operation::ListDuringDownload { pacing } => {
                let handle = shared_file.expect("uploaded before the loop");
                let t = backend
                    .download_with_mid_list(handle, file_size, pacing.to_download_pacing())
                    .await?;
                if t.bytes != file_size {
                    return Err(format!("read {} bytes, expected {file_size}", t.bytes).into());
                }
                eprintln!(
                    "{label} listing issued at {}, answered in {:.0} ms, at {}",
                    format_bytes(t.issued_at),
                    t.list_latency.as_secs_f64() * 1000.0,
                    format_bytes(t.answered_at)
                );
                if !is_warmup {
                    list_latencies.push(t.list_latency);
                }
                t.download
            }
        };

        eprintln!("{} {:.3}s", label, elapsed.as_secs_f64());

        if !is_warmup {
            durations.push(elapsed);
        }
    }

    if let Some(handle) = shared_file {
        backend.delete(handle).await?;
    }
    backend.cleanup().await?;

    let mut result = BenchmarkResult::new(BackendName::MtpRs, operation, file_size, durations);
    result.list_latencies = list_latencies;
    Ok(result)
}

/// Run a single scenario with the libmtp (sync) backend.
/// This function is meant to be called from `spawn_blocking`.
fn run_libmtp_scenario(
    serial: Option<&str>,
    mtp_serial: Option<&str>,
    operation: Operation,
    file_size: u64,
    num_runs: usize,
    warmup_runs: usize,
) -> Result<BenchmarkResult, Box<dyn std::error::Error + Send + Sync>> {
    let test_data = vec![0xABu8; file_size as usize];
    let total_iterations = warmup_runs + num_runs;
    let mut durations = Vec::with_capacity(num_runs);

    eprintln!("  Connecting (libmtp)...");
    let backend = LibmtpBackend::connect(serial, mtp_serial)
        .map_err(|e| format!("libmtp connect failed: {e}"))?;
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
            | Operation::DownloadWindowed { .. }
            | Operation::ListDuringDownload { .. } => {
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

    Ok(BenchmarkResult::new(
        BackendName::Libmtp,
        operation,
        file_size,
        durations,
    ))
}
