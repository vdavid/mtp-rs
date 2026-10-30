//! Read a large file window-by-window without monopolizing the MTP session.
//!
//! Demonstrates `Storage::download_windowed`: read a file as a sequence of small
//! bounded windows, and BETWEEN windows run another device operation (a folder
//! listing) to prove the one-per-device PTP session is free the whole time.
//! A plain `download(handle, ByteRange::Full)` would hold that session open for the
//! entire file, so the listing couldn't run until the read finished or was cancelled.
//!
//! Finally it reassembles the windows and verifies they're byte-exact against a
//! plain full download.
//!
//! This runs against the in-process virtual device, so it needs no hardware:
//!
//! ```text
//! cargo run --example windowed_download --features virtual-device
//! ```

#[cfg(not(feature = "virtual-device"))]
fn main() {
    eprintln!("Run with: cargo run --example windowed_download --features virtual-device");
}

#[cfg(feature = "virtual-device")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use mtp_rs::mtp::MtpDevice;
    use mtp_rs::{VirtualDeviceConfig, VirtualStorageConfig};
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
            serial: "windowed-demo".into(),
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

    // Read in 32 KiB windows. Each next_window() is one bounded transaction that
    // releases the session on return; pick a window size to taste (the
    // DEFAULT_DOWNLOAD_WINDOW of 8 MiB is the documented suggestion for real
    // hardware; here a small window just makes the loop visibly iterate).
    let window_size = 32 * 1024;
    let mut download = storage
        .download_windowed(obj.handle, mtp_rs::ByteRange::Full, window_size)
        .await?;
    // size() reports the FULL object size, so progress stays anchored.
    assert_eq!(download.size(), obj.size);

    let mut assembled = Vec::with_capacity(obj.size as usize);
    let mut window_index = 0;
    let mut listings_between = 0;
    while let Some(window) = download.next_window().await {
        let bytes = window?;
        assembled.extend_from_slice(&bytes);
        window_index += 1;
        println!(
            "Window {window_index}: +{} bytes ({}/{} total)",
            bytes.len(),
            assembled.len(),
            obj.size
        );

        // The session is FREE right here. Run a real device op BETWEEN windows.
        // A held-open `download(..., ByteRange::Full)` couldn't do this. The consumer interposes
        // whatever policy it wants here; the library holds no session lock.
        let listed = storage.list_objects(None).await?;
        listings_between += 1;
        println!(
            "  ...listed {} object(s) on the device between windows. Session is free.",
            listed.len()
        );
    }
    println!(
        "\nRead {} bytes across {} windows, with {} listings interleaved.",
        assembled.len(),
        window_index,
        listings_between
    );

    // Verify the reassembled file matches a plain full download (and the source).
    let full = storage.download_to_vec(obj.handle).await?;
    assert_eq!(
        assembled, full,
        "windowed reassembly must equal a full download"
    );
    assert_eq!(assembled, content, "assembled bytes must equal source");
    println!("\n✓ Windowed reassembly matches the full download exactly.");

    // No cancel() needed: WindowedDownload holds nothing between windows, so
    // dropping it (here, at end of scope) is a clean no-op.
    Ok(())
}
