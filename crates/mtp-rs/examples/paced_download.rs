//! Read a file in windows, using the device for something else in between.
//!
//! `Storage::download` reads a file as a series of windows (by default, each one takes about
//! two seconds) and frees the device's one PTP session between them. This demo uses small fixed
//! windows so the loop visibly iterates, runs a folder listing whenever no window is in flight
//! (`FileDownload::holds_session()` is `false`), and checks the result against a full read.
//!
//! This runs against the in-process virtual device, so it needs no hardware:
//!
//! ```text
//! cargo run --example paced_download --features virtual-device
//! ```

#[cfg(not(feature = "virtual-device"))]
fn main() {
    eprintln!("Run with: cargo run --example paced_download --features virtual-device");
}

#[cfg(feature = "virtual-device")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use mtp_rs::mtp::MtpDevice;
    use mtp_rs::virtual_device::{VirtualDeviceConfig, VirtualStorageConfig};
    use mtp_rs::{ByteRange, DownloadPacing};
    use std::time::Duration;

    // Back the virtual device with a temp dir holding one ~256 KiB file with a
    // recognizable byte pattern, plus a sibling so the in-between listing has
    // something to report.
    let dir = tempfile::tempdir()?;
    let content: Vec<u8> = (0..256 * 1024).map(|i| (i % 256) as u8).collect();
    std::fs::write(dir.path().join("movie.bin"), &content)?;
    std::fs::write(dir.path().join("notes.txt"), b"a sibling file")?;

    let device = MtpDevice::builder()
        .open_virtual(VirtualDeviceConfig {
            serial: "paced-demo".into(),
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
    let obj = storage
        .list_objects(None)
        .await?
        .into_iter()
        .find(|o| o.filename == "movie.bin")
        .expect("movie.bin should be listed");
    println!("Source file: {} ({} bytes)\n", obj.filename, obj.size);

    // 32 KiB windows, so the loop visibly iterates. On real hardware, `storage.download(..)`
    // with the default pacing is the usual choice.
    let mut download = storage
        .download_with_pacing(
            obj.handle,
            ByteRange::Full,
            DownloadPacing::FixedWindow(32 * 1024),
        )
        .await?;
    // size() reports the FULL object size, so progress stays anchored.
    assert_eq!(download.size(), obj.size);

    let mut assembled = Vec::with_capacity(obj.size as usize);
    let mut listings_between = 0;
    while let Some(chunk) = download.next_chunk().await {
        assembled.extend_from_slice(&chunk?);
        if download.holds_session() {
            continue;
        }
        // No window in flight: the session is free, so run a real device op right here.
        let listed = storage.list_objects(None).await?;
        listings_between += 1;
        println!(
            "{}/{} bytes; listed {} object(s) between windows.",
            assembled.len(),
            obj.size,
            listed.len()
        );
    }
    println!(
        "\nRead {} bytes, with {} listings interleaved.",
        assembled.len(),
        listings_between
    );

    // Verify the reassembled file matches a plain full download (and the source).
    let full = storage.download_to_vec(obj.handle).await?;
    assert_eq!(assembled, full, "paced download must equal a full download");
    assert_eq!(assembled, content, "assembled bytes must equal source");
    println!("\n✓ Paced download matches the full download exactly.");
    Ok(())
}
