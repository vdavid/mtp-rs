//! Resume a streaming download from a byte offset.
//!
//! Demonstrates `Storage::download_stream_from_offset`: download half a file,
//! stop (cancelling the stream, which frees the one-per-device MTP session so
//! the device can be navigated), then resume from the kept byte count and append
//! the rest. Verifies the reassembled file matches a plain full download.
//!
//! This runs against the in-process virtual device, so it needs no hardware:
//!
//! ```text
//! cargo run --example resume_download --features virtual-device
//! ```

#[cfg(not(feature = "virtual-device"))]
fn main() {
    eprintln!("Run with: cargo run --example resume_download --features virtual-device");
}

#[cfg(feature = "virtual-device")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use mtp_rs::mtp::{MtpDevice, DEFAULT_CANCEL_TIMEOUT};
    use mtp_rs::{VirtualDeviceConfig, VirtualStorageConfig};
    use std::time::Duration;

    // Back the virtual device with a temp dir holding one ~256 KiB file with a
    // recognizable byte pattern.
    let dir = tempfile::tempdir()?;
    let content: Vec<u8> = (0..256 * 1024).map(|i| (i % 256) as u8).collect();
    std::fs::write(dir.path().join("movie.bin"), &content)?;

    let device = MtpDevice::builder()
        .open_virtual(VirtualDeviceConfig {
            serial: "resume-demo".into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: dir.path().to_path_buf(),
                read_only: false,
            }],
            // No watcher, no event sleep: this demo doesn't need either.
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        })
        .await?;

    let storages = device.storages().await?;
    let storage = &storages[0];
    let obj = storage.list_objects(None).await?[0].clone();
    println!("Source file: {} ({} bytes)\n", obj.filename, obj.size);

    // Pick a resume point partway through the file.
    let kept: u64 = obj.size / 2;

    // --- Phase 1: stream the prefix [0, kept), then stop to free the session. ---
    //
    // Real-world driver: pull chunks, write them to a temp, and once `kept` bytes
    // are on disk, cancel. cancel() drains the USB pipe and releases the
    // one-per-device MTP session, so the device can be navigated while "paused".
    let mut assembled = Vec::with_capacity(obj.size as usize);
    let mut download = storage
        .download(obj.handle, mtp_rs::ByteRange::Full)
        .await?;
    while (assembled.len() as u64) < kept {
        let Some(chunk) = download.next_chunk().await else {
            break;
        };
        assembled.extend_from_slice(&chunk?);
    }
    // The virtual device sends the whole object in one data container, so a single
    // chunk can overshoot `kept`. Trim to exactly the prefix we mean to keep.
    // A real per-chunk driver would simply stop at a chunk boundary near `kept`.
    assembled.truncate(kept as usize);
    println!(
        "Kept {} bytes, then pausing (cancel releases the MTP session)...",
        assembled.len()
    );
    download.cancel(DEFAULT_CANCEL_TIMEOUT).await?;
    drop(download);

    // Prove the session is usable while paused.
    let listed = storage.list_objects(None).await?;
    println!(
        "While paused, listed {} object(s) on the device. The session is free.\n",
        listed.len()
    );

    // --- Phase 2: resume from the kept offset and append the rest. ---
    println!("Resuming from offset {kept}...");
    let mut resumed = storage
        .download(obj.handle, mtp_rs::ByteRange::From(kept))
        .await?;
    // size() reports the FULL object size, even on a resume, so progress stays
    // anchored to the whole file.
    assert_eq!(resumed.size(), obj.size);
    while let Some(chunk) = resumed.next_chunk().await {
        assembled.extend_from_slice(&chunk?);
    }
    drop(resumed);
    println!("Resume complete. Assembled {} bytes.\n", assembled.len());

    // --- Verify the assembled file (prefix + resumed tail) matches the source. ---
    let full = storage
        .download(obj.handle, mtp_rs::ByteRange::Full)
        .await?
        .collect()
        .await?;
    assert_eq!(
        assembled, full,
        "prefix + resumed tail must equal a full download"
    );
    assert_eq!(assembled, content, "assembled bytes must equal source");
    println!("✓ Assembled file (prefix + resumed tail) matches the source exactly.");

    Ok(())
}
