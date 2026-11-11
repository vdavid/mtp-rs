//! Upload step by step under an app's own device lock, letting other work in between windows.
//!
//! An app that serializes its calls to a device with its own lock can't use `Storage::upload`
//! and still let a folder listing in during a big upload: the upload is one call, so the app
//! holds its lock for all of it. `Storage::start_upload` returns a `FileUpload` that the app
//! feeds chunk by chunk instead. `holds_session()` says when a transfer is in flight: the app
//! holds its lock only then, and between windows a waiting listing gets the device.
//!
//! This runs against the in-process virtual device, which models Android's edit operations, so
//! it needs no hardware:
//!
//! ```text
//! cargo run --example upload_with_app_lock --features virtual-device
//! ```

#[cfg(not(feature = "virtual-device"))]
fn main() {
    eprintln!("Run with: cargo run --example upload_with_app_lock --features virtual-device");
}

#[cfg(feature = "virtual-device")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use bytes::Bytes;
    use futures::lock::Mutex;
    use mtp_rs::mtp::MtpDevice;
    use mtp_rs::virtual_device::{VirtualDeviceConfig, VirtualStorageConfig};
    use mtp_rs::{NewObjectInfo, UploadPacing};
    use std::sync::Arc;
    use std::time::Duration;

    const WINDOW: u32 = 64 * 1024;

    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("notes.txt"), b"a sibling file")?;
    let device = MtpDevice::builder()
        .open_virtual(VirtualDeviceConfig {
            serial: "app-lock-demo".into(),
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

    // The app's own lock: one call on the device at a time, as a file manager might keep it.
    let app_lock = Arc::new(Mutex::new(()));
    let storage = device.storages().await?.remove(0);

    // A browsing task that wants the device while the upload runs.
    let browser = {
        let (app_lock, storage) = (Arc::clone(&app_lock), device.storages().await?.remove(0));
        tokio::spawn(async move {
            for _ in 0..3 {
                let _guard = app_lock.lock().await;
                let listed = storage.list_objects(None).await.map(|l| l.len());
                println!("    browser: listed {listed:?} object(s)");
                drop(_guard);
                tokio::task::yield_now().await;
            }
        })
    };

    let content: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    let mut upload = storage
        .start_upload(
            None,
            NewObjectInfo::file("movie.bin", content.len() as u64),
            UploadPacing::FixedWindow(WINDOW),
        )
        .await?;
    let mut guard = None;
    for piece in content.chunks(16 * 1024) {
        if guard.is_none() {
            guard = Some(app_lock.lock().await);
        }
        upload.write_chunk(Bytes::copy_from_slice(piece)).await?;
        if !upload.holds_session() {
            // A window just closed: let other work in before the next one.
            println!("upload: {} bytes written", upload.bytes_written());
            guard = None;
            tokio::task::yield_now().await;
        }
    }
    let handle = upload.finish().await?;
    drop(guard);
    browser.await?;

    assert_eq!(storage.download_to_vec(handle).await?, content);
    println!(
        "\nUploaded {} bytes; the browser listed between windows.",
        content.len()
    );
    Ok(())
}
