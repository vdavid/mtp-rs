//! Write a file in windows while another task uses the device in between, then stop one partway.
//!
//! On a device that lists Android's edit operations, `Storage::upload` writes a file as a series
//! of windows (by default, each one takes about two seconds) and frees the device's one PTP
//! session between them, so a folder listing from another task waits for one window instead of
//! the whole file. This demo uses small fixed windows so the windows are visible, lists the
//! folder from a second task during the upload, and prints the order things happened in. Then it
//! stops an upload from its progress callback and shows what's left: exactly the windows the
//! device acknowledged, under a handle the library hands back and never deletes.
//!
//! This runs against the in-process virtual device, which models Android's edit operations, so
//! it needs no hardware:
//!
//! ```text
//! cargo run --example paced_upload --features virtual-device
//! ```

#[cfg(not(feature = "virtual-device"))]
fn main() {
    eprintln!("Run with: cargo run --example paced_upload --features virtual-device");
}

#[cfg(feature = "virtual-device")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use bytes::Bytes;
    use mtp_rs::mtp::MtpDevice;
    use mtp_rs::virtual_device::{VirtualDeviceConfig, VirtualStorageConfig};
    use mtp_rs::{NewObjectInfo, UploadPacing};
    use std::ops::ControlFlow;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const WINDOW: u32 = 64 * 1024;

    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("notes.txt"), b"a sibling file")?;
    let device = MtpDevice::builder()
        .open_virtual(VirtualDeviceConfig {
            serial: "paced-upload-demo".into(),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: dir.path().to_path_buf(),
                read_only: false,
            }],
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        })
        .await?;
    println!(
        "Windowed uploads supported: {}\n",
        device.capabilities().supports_windowed_upload
    );

    let content: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    let chunks = |data: &[u8]| {
        let chunks: Vec<_> = data
            .chunks(16 * 1024)
            .map(|c| Ok::<_, std::io::Error>(Bytes::copy_from_slice(c)))
            .collect();
        futures::stream::iter(chunks)
    };

    // A second task lists the root until the upload is done. Both write to one timeline.
    let timeline = Arc::new(Mutex::new(Vec::<String>::new()));
    let done = Arc::new(AtomicBool::new(false));
    let lister = {
        let storage = device.storages().await?.remove(0);
        let (timeline, done) = (Arc::clone(&timeline), Arc::clone(&done));
        tokio::spawn(async move {
            while !done.load(Ordering::Relaxed) {
                let listed = storage.list_objects(None).await.map(|l| l.len());
                timeline
                    .lock()
                    .unwrap()
                    .push(format!("    listing: {listed:?} object(s)"));
                tokio::task::yield_now().await;
            }
        })
    };

    let storage = device.storages().await?.remove(0);
    let progress_log = Arc::clone(&timeline);
    let handle = storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("movie.bin", content.len() as u64),
            chunks(&content),
            UploadPacing::FixedWindow(WINDOW),
            move |p| {
                if p.bytes_transferred % u64::from(WINDOW) == 0 {
                    progress_log.lock().unwrap().push(format!(
                        "upload: {} of {} bytes read",
                        p.bytes_transferred,
                        p.total_bytes.unwrap_or(0)
                    ));
                }
                ControlFlow::Continue(())
            },
        )
        .await?;
    done.store(true, Ordering::Relaxed);
    lister.await?;
    for line in timeline.lock().unwrap().iter() {
        println!("{line}");
    }
    let back = storage.download_to_vec(handle).await?;
    assert_eq!(back, content, "the upload must arrive whole");
    println!(
        "\nUploaded {} bytes in {}-byte windows; the listings ran between them.\n",
        back.len(),
        WINDOW
    );

    // Stop an upload partway: the object keeps exactly the acknowledged windows.
    let stop_at = 2 * u64::from(WINDOW) + 1000;
    let err = storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("stopped.bin", content.len() as u64),
            chunks(&content),
            UploadPacing::FixedWindow(WINDOW),
            |p| {
                if p.bytes_transferred >= stop_at {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )
        .await
        .expect_err("the progress callback stops it");
    let partial = err.partial.expect("a windowed upload names what it left");
    let kept = storage.download_to_vec(partial).await?;
    println!(
        "Stopped at {stop_at} bytes read ({}): the device kept {} bytes, the first {} windows.",
        err.source,
        kept.len(),
        kept.len() / WINDOW as usize
    );
    assert_eq!(kept, content[..kept.len()]);
    storage.delete(partial).await?;
    println!("Deleted the partial; the library never does that on its own.");
    Ok(())
}
