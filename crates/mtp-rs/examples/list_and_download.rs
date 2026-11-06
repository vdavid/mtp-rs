//! List files and download one.
//!
//! Run with: cargo run --example list_and_download

use mtp_rs::mtp::MtpDevice;
use mtp_rs::{ByteRange, ObjectInfo};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== File listing and download test ===\n");

    let device = MtpDevice::open_first().await?;
    println!(
        "Connected to: {} {}\n",
        device.device_info().manufacturer,
        device.device_info().model
    );

    let storages = device.storages().await?;
    let storage = &storages[0];

    // List all files recursively
    println!("=== All files ===");
    let objects = storage.list_objects_recursive(None).await?;

    let mut first_image: Option<ObjectInfo> = None;

    for obj in &objects {
        let kind = if obj.is_folder() { "DIR " } else { "FILE" };
        println!(
            "  {} {} ({} bytes) [handle: {:?}]",
            kind, obj.filename, obj.size, obj.handle
        );

        // Find first image file
        if first_image.is_none() && obj.is_file() {
            let ext = obj.filename.to_lowercase();
            if ext.ends_with(".jpg") || ext.ends_with(".jpeg") || ext.ends_with(".raf") {
                first_image = Some(obj.clone());
            }
        }
    }

    println!("\nTotal: {} objects\n", objects.len());

    // Try to download the first image
    if let Some(img) = first_image {
        println!("=== Downloading: {} ({} bytes) ===", img.filename, img.size);

        // `download` reads in windows of about two seconds each and frees the device's
        // single PTP session between them, so other operations (listings, navigation)
        // stay responsive and a cancel stops quickly. `download_with_pacing` picks another
        // pacing, like one held-open transfer.
        let start = std::time::Instant::now();
        let mut download = storage.download(img.handle, ByteRange::Full).await?;
        let total_size = download.size();
        let mut data = Vec::with_capacity(total_size as usize);
        while let Some(chunk) = download.next_chunk().await {
            data.extend_from_slice(&chunk?);
        }
        let elapsed = start.elapsed();

        let speed = if elapsed.as_secs_f64() > 0.0 {
            (data.len() as f64 / 1024.0 / 1024.0) / elapsed.as_secs_f64()
        } else {
            0.0
        };

        println!(
            "✓ Downloaded {} bytes in {:.2}s ({:.2} MB/s)",
            data.len(),
            elapsed.as_secs_f64(),
            speed
        );
        assert_eq!(data.len() as u64, total_size, "Size mismatch!");

        // Save to temp file
        let path = format!("/tmp/{}", img.filename);
        std::fs::write(&path, &data)?;
        println!("✓ Saved to {}", path);
    } else {
        println!("No image files found to download");
    }

    println!("\n=== Done ===");
    Ok(())
}
