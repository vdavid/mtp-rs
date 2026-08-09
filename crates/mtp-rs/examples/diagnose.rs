//! Diagnostic script to investigate MTP issues.
//!
//! Run with: cargo run --example diagnose

use bytes::Bytes;
use mtp_rs::mtp::{ListingItem, MtpDevice, NewObjectInfo};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== MTP Diagnostic Tool ===\n");

    // Connect to device
    let device = MtpDevice::open_first().await?;
    println!(
        "Connected to: {} {}",
        device.device_info().manufacturer,
        device.device_info().model
    );

    let storages = device.storages().await?;
    let storage = &storages[0];
    println!("Storage: {}\n", storage.info().description);

    // Test 1: List root objects (non-recursive)
    println!("=== Test 1: Root folder listing (non-recursive) ===");
    let root_objects = storage.list_objects(None).await?;
    let root_folders = root_objects.iter().filter(|o| o.is_folder()).count();
    let root_files = root_objects.iter().filter(|o| o.is_file()).count();
    println!(
        "Root contains: {} folders, {} files, {} total\n",
        root_folders,
        root_files,
        root_objects.len()
    );

    // Test 2: Recursive traversal, bounded.
    //
    // Capped at MAX_OBJECTS: per-object metadata fetches take ~1s each on
    // some PTP cameras, so a full recursive listing can run 10+ minutes.
    // Diagnosis needs to know whether recursion works, not the exact totals.
    // The cap also removes the temptation to Ctrl+C mid-traversal, which
    // leaves some devices (Panasonic Lumix DMC-TZ61, issue #12) stuck until
    // a reset.
    const MAX_OBJECTS: usize = 200;
    println!(
        "=== Test 2: Recursive listing (bounded to {} objects) ===",
        MAX_OBJECTS
    );
    let start = std::time::Instant::now();
    let mut rec_folders = 0usize;
    let mut rec_files = 0usize;
    let mut rec_total = 0usize;
    let mut rec_skipped = 0usize;
    let mut capped = false;
    let mut to_visit = std::collections::VecDeque::from([None]);
    'traversal: while let Some(parent) = to_visit.pop_front() {
        let mut listing = storage.list_objects_stream(parent).await?;
        while let Some(result) = listing.next().await {
            let obj = match result? {
                ListingItem::Object(info) => info,
                // Worth counting rather than ignoring: an object the device
                // wouldn't describe is exactly the kind of thing a diagnostic run
                // exists to surface.
                ListingItem::Skipped(skipped) => {
                    println!(
                        "  ! handle {} could not be read: {}",
                        skipped.handle.0, skipped.error
                    );
                    rec_skipped += 1;
                    continue;
                }
            };
            rec_total += 1;
            if obj.is_folder() {
                rec_folders += 1;
                to_visit.push_back(Some(obj.handle));
            } else {
                rec_files += 1;
            }
            if rec_total >= MAX_OBJECTS {
                capped = true;
                break 'traversal;
            }
        }
    }
    let elapsed = start.elapsed();
    println!(
        "Recursive traversal saw: {} folders, {} files, {} total{}{}",
        rec_folders,
        rec_files,
        rec_total,
        if rec_skipped > 0 {
            format!(", {rec_skipped} unreadable")
        } else {
            String::new()
        },
        if capped {
            " (stopped at cap, more exist)"
        } else {
            ""
        }
    );
    println!("Time taken: {:.2}s\n", elapsed.as_secs_f64());

    // Test 3: Manual recursive listing of first folder
    if let Some(first_folder) = root_objects.iter().find(|o| o.is_folder()) {
        println!(
            "=== Test 3: Listing contents of '{}' folder ===",
            first_folder.filename
        );
        let folder_contents = storage.list_objects(Some(first_folder.handle)).await?;
        let sub_folders = folder_contents.iter().filter(|o| o.is_folder()).count();
        let sub_files = folder_contents.iter().filter(|o| o.is_file()).count();
        println!(
            "'{}' contains: {} folders, {} files, {} total\n",
            first_folder.filename,
            sub_folders,
            sub_files,
            folder_contents.len()
        );

        // Show first few items
        for (i, obj) in folder_contents.iter().take(5).enumerate() {
            let kind = if obj.is_folder() { "DIR" } else { "FILE" };
            println!(
                "  {}. {} {} ({} bytes)",
                i + 1,
                kind,
                obj.filename,
                obj.size
            );
        }
        if folder_contents.len() > 5 {
            println!("  ... and {} more", folder_contents.len() - 5);
        }
        println!();
    }

    // Test 4: Find and download a small file
    println!("=== Test 4: Download test ===");
    let small_file = root_objects
        .iter()
        .find(|o| o.is_file() && o.size > 1000 && o.size < 100_000);

    match small_file {
        Some(file) => {
            println!("Downloading: {} ({} bytes)", file.filename, file.size);
            let data = storage.download_to_vec(file.handle).await?;
            println!("Downloaded {} bytes successfully!", data.len());

            // Verify size matches
            if data.len() as u64 == file.size {
                println!("✓ Size matches expected");
            } else {
                println!(
                    "✗ Size mismatch: expected {}, got {}",
                    file.size,
                    data.len()
                );
            }
        }
        None => {
            println!("No suitable small file found in root, checking subfolders...");

            // Try to find a file in a subfolder
            for folder in root_objects.iter().filter(|o| o.is_folder()).take(5) {
                let contents = storage.list_objects(Some(folder.handle)).await?;
                if let Some(file) = contents
                    .iter()
                    .find(|o| o.is_file() && o.size > 1000 && o.size < 100_000)
                {
                    println!(
                        "Found file in '{}': {} ({} bytes)",
                        folder.filename, file.filename, file.size
                    );
                    let data = storage.download_to_vec(file.handle).await?;
                    println!("Downloaded {} bytes successfully!", data.len());

                    if data.len() as u64 == file.size {
                        println!("✓ Size matches expected");
                    } else {
                        println!(
                            "✗ Size mismatch: expected {}, got {}",
                            file.size,
                            data.len()
                        );
                    }
                    break;
                }
            }
        }
    }

    // Test 5: Upload test
    println!("\n=== Test 5: Upload test ===");

    // Try uploading to the Download folder (more likely to work)
    let download_folder = root_objects.iter().find(|o| o.filename == "Download");

    match download_folder {
        Some(folder) => {
            println!("Uploading to Download folder (handle: {:?})", folder.handle);

            let test_content = b"Test file from mtp-rs diagnostic";
            let info = NewObjectInfo::file("mtp-rs-diag-test.txt", test_content.len() as u64);
            let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(
                test_content.to_vec(),
            ))]);

            match storage
                .upload(Some(folder.handle), info, Box::pin(stream))
                .await
            {
                Ok(handle) => {
                    println!("✓ Upload succeeded! Handle: {:?}", handle);

                    // Clean up - delete the file
                    println!("Cleaning up...");
                    match storage.delete(handle).await {
                        Ok(_) => println!("✓ Cleanup successful"),
                        Err(e) => println!("✗ Cleanup failed: {}", e),
                    }
                }
                Err(e) => {
                    println!("✗ Upload to Download folder failed: {}", e);

                    // Try uploading to root
                    println!("\nTrying upload to root...");
                    let info2 =
                        NewObjectInfo::file("mtp-rs-diag-test.txt", test_content.len() as u64);
                    let stream2 = futures::stream::iter(vec![Ok::<_, std::io::Error>(
                        Bytes::from(test_content.to_vec()),
                    )]);

                    match storage.upload(None, info2, Box::pin(stream2)).await {
                        Ok(handle) => {
                            println!("✓ Upload to root succeeded! Handle: {:?}", handle);
                            let _ = storage.delete(handle).await;
                        }
                        Err(e2) => println!("✗ Upload to root also failed: {}", e2),
                    }
                }
            }
        }
        None => {
            println!("Download folder not found, trying root...");
            let test_content = b"Test file from mtp-rs diagnostic";
            let info = NewObjectInfo::file("mtp-rs-diag-test.txt", test_content.len() as u64);
            let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(
                test_content.to_vec(),
            ))]);

            match storage.upload(None, info, Box::pin(stream)).await {
                Ok(handle) => {
                    println!("✓ Upload succeeded! Handle: {:?}", handle);
                    let _ = storage.delete(handle).await;
                }
                Err(e) => println!("✗ Upload failed: {}", e),
            }
        }
    }

    println!("\n=== Diagnostics complete ===");
    Ok(())
}
