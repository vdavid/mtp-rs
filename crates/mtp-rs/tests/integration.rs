//! Integration tests for mtp-rs.
//!
//! Requires a real MTP device (Android phone, Kindle, Garmin watch, etc.)
//! connected via USB. MTP only supports one operation at a time, so use
//! `--test-threads=1`.
//!
//! ```sh
//! # Read-only tests (safe):
//! cargo test --test integration readonly -- --ignored --nocapture --test-threads=1
//!
//! # Destructive tests (writes to device):
//! cargo test --test integration destructive -- --ignored --nocapture --test-threads=1
//!
//! # All tests (skip slow ones):
//! cargo test --test integration -- --ignored --nocapture --test-threads=1 --skip slow
//! ```
//!
//! One test is opt-in: `test_drop_mid_stream_then_software_reconnect` poisons
//! the session on purpose and can wedge some camera firmware until a USB
//! replug, so it only runs with `MTP_RUN_DROP_RECOVERY=1` (and `--release`).
//!
//! ## Picking a writable folder
//!
//! Destructive tests need a folder they can write into. By default they walk a
//! priority list of common folder names (`Download`, `Downloads`, `Music`,
//! `Documents`, `documents`, `Pictures`, `Audiobooks`, `Podcasts`) and use the
//! first one that exists in the storage root. If your device exposes a
//! differently-named folder, override the list with `MTP_TEST_FOLDER`:
//!
//! ```sh
//! MTP_TEST_FOLDER=Internal cargo test --test integration destructive -- --ignored ...
//! ```
//!
//! When no match is found, destructive tests skip with a clear log line.
//!
//! ## Picking a file for download tests
//!
//! Download tests search for a file in a size range: common folders first,
//! then a recursive streaming search that stops at the first match. The find
//! is cached across tests in the run. On devices where even that is slow, or
//! to pin a specific file, set `MTP_TEST_READFILE` to a `/`-separated path
//! from the storage root and no searching happens at all:
//!
//! ```sh
//! MTP_TEST_READFILE=/DCIM/111_PANA/P1110001.JPG cargo test --test integration -- --ignored ...
//! ```
//!
//! ## The >4 GiB round trip
//!
//! `test_big_file_over_4gib_round_trip` needs a second gate on top of
//! `#[ignore]`: `MTP_TEST_BIG_FILE=1`. Without it the test returns early, so a
//! plain `--ignored` sweep never writes gigabytes to someone's phone. It
//! defaults to 4 GiB + 64 MiB, overridable in bytes via
//! `MTP_TEST_BIG_FILE_SIZE`:
//!
//! ```sh
//! MTP_TEST_BIG_FILE=1 cargo test --test integration -- --ignored --nocapture big_file
//! ```

use mtp_rs::mtp::Storage;
use mtp_rs::{ByteRange, ListingItem, ObjectHandle};

/// Whether an opt-in env flag is enabled. True only for truthy values
/// (`1`/`true`/`yes`/`on`, case-insensitive), so `VAR=0` means off, not "defined
/// so on" (which is what a bare presence check would wrongly do).
fn env_enabled(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Bulk-transfer timeout for E2E device opens, read from `MTP_TEST_TIMEOUT_SECS`
/// (default 30). Set it low (for example `MTP_TEST_TIMEOUT_SECS=2`) so a wedged
/// or absent device skips fast instead of stalling 30s per operation. The
/// library default is unchanged, so consumers and CI keep 30s unless they opt in.
fn test_timeout() -> std::time::Duration {
    let secs = std::env::var("MTP_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(30);
    std::time::Duration::from_secs(secs)
}

/// Open the first MTP device with the E2E test timeout (see [`test_timeout`]).
async fn open_test_mtp() -> Result<mtp_rs::mtp::MtpDevice, mtp_rs::Error> {
    mtp_rs::mtp::MtpDevice::builder()
        .timeout(test_timeout())
        .open_first()
        .await
}

/// Open the first PTP device with the E2E test timeout (see [`test_timeout`]).
async fn open_test_ptp() -> Result<mtp_rs::ptp::PtpDevice, mtp_rs::PtpError> {
    mtp_rs::ptp::PtpDevice::open_first_with_timeout(test_timeout()).await
}
use serial_test::serial;
use std::time::Instant;

/// Global test start time - initialized lazily on first use
static TEST_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Get the elapsed time since tests started, formatted as [HH:MM:SS.mmm]
fn elapsed_timestamp() -> String {
    let start = TEST_START.get_or_init(Instant::now);
    let elapsed = start.elapsed();
    let total_secs = elapsed.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    let millis = elapsed.subsec_millis();
    format!("[{:02}:{:02}:{:02}.{:03}]", hours, minutes, seconds, millis)
}

/// Timestamped logging macro
macro_rules! tlog {
    ($($arg:tt)*) => {{
        // Initialize start time on first log
        let _ = TEST_START.get_or_init(Instant::now);
        println!("{} {}", $crate::elapsed_timestamp(), format_args!($($arg)*));
    }};
}

/// Handle device errors gracefully - skip test on hardware issues, panic on others.
macro_rules! try_device {
    ($expr:expr, $context:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => {
                // Normalize to the neutral error so the helpers work for both
                // high-level (`mtp::Error`) and low-level (`ptp::PtpError`) calls.
                let e: mtp_rs::Error = e.into();
                if is_hardware_error(&e) {
                    tlog!("SKIPPING: {} - {:?}", $context, e);
                    print_device_help(&e);
                    return;
                } else {
                    panic!("{} failed: {:?}", $context, e);
                }
            }
        }
    };
}

/// Borrow the first storage, or skip the test cleanly when the device reports
/// none. An opened device with zero storages is a half-authorized or still-
/// settling phone (unlock it and grant "Allow access to phone data"), not a test
/// failure, so skip like a hardware error instead of panicking on `storages[0]`.
macro_rules! first_storage {
    (mut $storages:expr) => {
        match $storages.first_mut() {
            Some(s) => s,
            None => {
                tlog!("SKIPPING: device reports no storages (unlock the phone and grant 'Allow access to phone data')");
                return;
            }
        }
    };
    ($storages:expr) => {
        match $storages.first() {
            Some(s) => s,
            None => {
                tlog!("SKIPPING: device reports no storages (unlock the phone and grant 'Allow access to phone data')");
                return;
            }
        }
    };
}

fn is_hardware_error(e: &mtp_rs::Error) -> bool {
    use mtp_rs::Error;
    matches!(e, Error::Timeout | Error::NoDevice | Error::Disconnected) || e.is_exclusive_access()
}

fn print_device_help(e: &mtp_rs::Error) {
    use mtp_rs::Error;
    match e {
        Error::Timeout => {
            tlog!("  Check: phone unlocked? USB authorized? Cable connected?");
        }
        Error::NoDevice => {
            tlog!("  Check: phone connected? Set to MTP/File Transfer mode?");
        }
        Error::Disconnected => {
            tlog!("  Check: cable secure? Phone didn't sleep?");
        }
        _ if e.is_exclusive_access() => {
            tlog!("  Close other apps (file managers, Photos, Android File Transfer)");
        }
        _ => {}
    }
}

/// Search common Android folders for a file in the given size range.
/// Returns (handle, size, filename) if found.
async fn find_file_in_common_folders(
    storage: &Storage,
    min_size: u64,
    max_size: u64,
) -> Option<(ObjectHandle, u64, String)> {
    let root_objects = storage.list_objects(None).await.ok()?;

    let common_folders = [
        "Download",
        "Downloads",
        "DCIM",
        "Pictures",
        "Music",
        "Documents",
    ];

    for folder_name in &common_folders {
        let Some(folder) = root_objects
            .iter()
            .find(|o| o.is_folder() && o.filename == *folder_name)
        else {
            continue;
        };

        let objects = storage
            .list_objects(Some(folder.handle))
            .await
            .unwrap_or_default();

        // For DCIM, also check Camera subfolder
        let objects_to_check = if *folder_name == "DCIM" {
            if let Some(camera) = objects
                .iter()
                .find(|o| o.is_folder() && o.filename == "Camera")
            {
                storage
                    .list_objects(Some(camera.handle))
                    .await
                    .unwrap_or_default()
            } else {
                objects
            }
        } else {
            objects
        };

        if let Some(f) = objects_to_check
            .iter()
            .find(|o| o.is_file() && o.size > min_size && o.size < max_size)
        {
            return Some((f.handle, f.size, f.filename.clone()));
        }
    }
    None
}

/// Cache of the last file found by [`find_suitable_file`], shared across
/// tests in one suite run. Searching can take 10+ minutes on devices with
/// slow per-object metadata fetches (PTP cameras), so doing it once is a big
/// win. The handle is re-verified against the device before reuse, since
/// every test opens its own session.
static FOUND_FILE_CACHE: std::sync::Mutex<Option<(ObjectHandle, u64, String)>> =
    std::sync::Mutex::new(None);

/// Resolve the `MTP_TEST_READFILE` override: a `/`-separated path from the
/// storage root, for example `/DCIM/111_PANA/P1110001.JPG`. Skips all
/// searching. Returns `None` (with a log line) when the path doesn't resolve.
async fn resolve_readfile_override(
    storage: &Storage,
    path: &str,
) -> Option<(ObjectHandle, u64, String)> {
    let mut parent: Option<ObjectHandle> = None;
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (last, folders) = segments.split_last()?;

    for segment in folders {
        let objects = match storage.list_objects(parent).await {
            Ok(o) => o,
            Err(e) => {
                tlog!("MTP_TEST_READFILE: listing '{}' failed: {:?}", segment, e);
                return None;
            }
        };
        let Some(folder) = objects
            .iter()
            .find(|o| o.is_folder() && o.filename == *segment)
        else {
            tlog!("MTP_TEST_READFILE: folder '{}' not found", segment);
            return None;
        };
        parent = Some(folder.handle);
    }

    // Stream the final folder so we can stop at the name match instead of
    // fetching metadata for every sibling.
    let mut listing = match storage.list_objects_stream(parent).await {
        Ok(l) => l,
        Err(e) => {
            tlog!("MTP_TEST_READFILE: listing final folder failed: {:?}", e);
            return None;
        }
    };
    while let Some(result) = listing.next().await {
        match result {
            Ok(ListingItem::Object(obj)) if obj.is_file() && obj.filename == *last => {
                return Some((obj.handle, obj.size, obj.filename));
            }
            Ok(ListingItem::Object(_)) => {}
            Ok(ListingItem::Skipped(skipped)) => {
                tlog!(
                    "MTP_TEST_READFILE: handle {} unreadable: {}",
                    skipped.handle.0,
                    skipped.error
                );
            }
            Err(e) => {
                tlog!("MTP_TEST_READFILE: object fetch failed: {:?}", e);
                return None;
            }
        }
    }
    tlog!("MTP_TEST_READFILE: file '{}' not found", last);
    None
}

/// Breadth-first streaming search with early exit: returns the first file in
/// the size range, checking objects as their metadata arrives instead of
/// listing the whole storage first. On cameras with hundreds of photos this
/// returns in seconds where a full recursive listing takes 10+ minutes.
async fn find_file_recursive_early_exit(
    storage: &Storage,
    min_size: u64,
    max_size: u64,
) -> Option<(ObjectHandle, u64, String)> {
    let mut to_visit: std::collections::VecDeque<Option<ObjectHandle>> =
        std::collections::VecDeque::from([None]);

    while let Some(parent) = to_visit.pop_front() {
        let mut listing = match storage.list_objects_stream(parent).await {
            Ok(l) => l,
            Err(e) => {
                tlog!("Listing folder failed: {:?}", e);
                continue;
            }
        };
        while let Some(result) = listing.next().await {
            match result {
                Ok(ListingItem::Object(obj)) => {
                    if obj.is_file() && obj.size > min_size && obj.size < max_size {
                        return Some((obj.handle, obj.size, obj.filename));
                    }
                    if obj.is_folder() {
                        to_visit.push_back(Some(obj.handle));
                    }
                }
                Ok(ListingItem::Skipped(skipped)) => {
                    tlog!("Handle {} unreadable: {}", skipped.handle.0, skipped.error);
                }
                Err(e) => {
                    tlog!("Object fetch failed: {:?}", e);
                    return None;
                }
            }
        }
    }
    None
}

/// Find a suitable file for download tests.
///
/// Resolution order:
/// 1. `MTP_TEST_READFILE` env override (exact path, no searching)
/// 2. Cache from an earlier test in this run (re-verified against the device)
/// 3. Common folders (fast)
/// 4. Recursive streaming search with early exit (slow, but stops at the
///    first match)
async fn find_suitable_file(
    storage: &Storage,
    min_size: u64,
    max_size: u64,
) -> Option<(ObjectHandle, u64, String)> {
    if let Ok(path) = std::env::var("MTP_TEST_READFILE") {
        tlog!("Using MTP_TEST_READFILE={}", path);
        let found = resolve_readfile_override(storage, &path).await?;
        if found.1 > min_size && found.1 < max_size {
            return Some(found);
        }
        tlog!(
            "MTP_TEST_READFILE file is {} bytes, outside the {}-{} range, skipping",
            found.1,
            min_size,
            max_size
        );
        return None;
    }

    // Reuse an earlier find if it fits this test's range and still resolves.
    let cached = FOUND_FILE_CACHE.lock().unwrap().clone();
    if let Some((handle, size, name)) = cached {
        if size > min_size && size < max_size {
            if let Ok(info) = storage.get_object_info(handle).await {
                if info.size == size && info.filename == name {
                    tlog!("Reusing cached file: {} ({} bytes)", name, size);
                    return Some((handle, size, name));
                }
            }
        }
    }

    let found = if let Some(result) = find_file_in_common_folders(storage, min_size, max_size).await
    {
        Some(result)
    } else {
        tlog!("No file in common folders, trying recursive search (early-exit)...");
        find_file_recursive_early_exit(storage, min_size, max_size).await
    };

    if let Some(f) = &found {
        *FOUND_FILE_CACHE.lock().unwrap() = Some(f.clone());
    }
    found
}

/// Find a writable folder at storage root for destructive tests.
///
/// If `MTP_TEST_FOLDER` is set, look for that folder name only.
/// Otherwise walk a priority list covering Android, Kindle, and Garmin
/// devices and return the first match. Returns `(handle, name)`.
async fn find_writable_folder(storage: &Storage) -> Option<(ObjectHandle, String)> {
    let root_objects = storage.list_objects(None).await.ok()?;

    let override_name = std::env::var("MTP_TEST_FOLDER").ok();
    let candidates: Vec<String> = if let Some(name) = &override_name {
        vec![name.clone()]
    } else {
        [
            "Download",   // Android (most common)
            "Downloads",  // Android (alt)
            "Music",      // Android, Garmin music-capable watches
            "Documents",  // Android (capitalized)
            "documents",  // Kindle (lowercase)
            "Pictures",   // Android
            "Audiobooks", // Garmin
            "Podcasts",   // Garmin
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
    };

    for name in &candidates {
        if let Some(folder) = root_objects
            .iter()
            .find(|o| o.is_folder() && o.filename == *name)
        {
            return Some((folder.handle, folder.filename.clone()));
        }
    }

    if let Some(name) = &override_name {
        tlog!(
            "MTP_TEST_FOLDER='{}' is set, but no folder by that name at storage root",
            name
        );
    }
    None
}

/// Read-only tests that don't modify the device.
mod readonly {
    use super::*;
    use mtp_rs::mtp::MtpDevice;
    use std::time::Duration;

    #[test]
    #[serial]
    fn test_list_devices() {
        let devices = MtpDevice::list_devices().expect("USB subsystem error");
        tlog!("Found {} MTP device(s)", devices.len());
        for dev in &devices {
            tlog!(
                "  {} {} ({:04x}:{:04x}) location={:08x}",
                dev.manufacturer.as_deref().unwrap_or("?"),
                dev.product.as_deref().unwrap_or("?"),
                dev.vendor_id,
                dev.product_id,
                dev.location_id
            );
        }
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_device_connection() {
        let device = try_device!(crate::open_test_mtp().await, "open device");
        let info = device.device_info();
        tlog!(
            "Connected: {} {} ({})",
            info.manufacturer,
            info.model,
            info.serial_number
        );
        assert!(!info.manufacturer.is_empty());
        assert!(!info.model.is_empty());
        device.close().await.expect("close failed");
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_list_storages() {
        let device = try_device!(crate::open_test_mtp().await, "open device");
        let storages = try_device!(device.storages().await, "get storages");
        tlog!("Found {} storage(s)", storages.len());
        // Skip cleanly rather than fail on a half-authorized/settling phone.
        let _ = first_storage!(storages);

        for storage in &storages {
            let info = storage.info();
            tlog!(
                "  {} - {:.2} GB free / {:.2} GB total",
                info.description,
                info.free_space as f64 / 1e9,
                info.total_capacity as f64 / 1e9
            );
        }
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_list_root_folder() {
        let device = try_device!(crate::open_test_mtp().await, "open device");
        let storages = try_device!(device.storages().await, "get storages");
        let storage = first_storage!(storages);

        let objects = try_device!(storage.list_objects(None).await, "list root");
        tlog!("Root contains {} objects", objects.len());

        for obj in objects.iter().take(20) {
            let kind = if obj.is_folder() { "DIR " } else { "FILE" };
            let size = if obj.is_folder() {
                "-".to_string()
            } else {
                format!("{}", obj.size)
            };
            tlog!("  {} {:>12} {}", kind, size, obj.filename);
        }
        if objects.len() > 20 {
            tlog!("  ... and {} more", objects.len() - 20);
        }

        assert!(objects.iter().any(|o| o.is_folder()));
    }

    /// SLOW: Lists ALL objects recursively. Set MTP_RUN_SLOW_TESTS=1 to run.
    #[tokio::test]
    #[ignore]
    #[serial]
    async fn slow_test_list_recursive() {
        if !env_enabled("MTP_RUN_SLOW_TESTS") {
            tlog!("SKIPPING slow_test_list_recursive (set MTP_RUN_SLOW_TESTS=1 to run)");
            return;
        }

        let device = try_device!(crate::open_test_mtp().await, "open device");
        let storages = try_device!(device.storages().await, "get storages");
        let storage = first_storage!(storages);

        tlog!("Starting recursive listing (may take several minutes)...");
        let objects = try_device!(storage.list_objects_recursive(None).await, "recursive list");

        let folders = objects.iter().filter(|o| o.is_folder()).count();
        let files = objects.iter().filter(|o| o.is_file()).count();
        tlog!(
            "Total: {} objects ({} folders, {} files)",
            objects.len(),
            folders,
            files
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_download_with_progress() {
        let device = try_device!(crate::open_test_mtp().await, "open device");
        let storages = try_device!(device.storages().await, "get storages");
        let storage = first_storage!(storages);

        tlog!("Searching for file (100KB-10MB)...");
        let Some((handle, file_size, file_name)) =
            find_suitable_file(storage, 100_000, 10_000_000).await
        else {
            tlog!("No suitable file found, skipping");
            return;
        };
        tlog!("Downloading {} ({} bytes)", file_name, file_size);

        let mut download = try_device!(
            storage.download(handle, ByteRange::Full).await,
            "start download"
        );
        let total = download.size();
        let mut last_percent = 0u64;

        while let Some(result) = download.next_chunk().await {
            result.expect("download error");
            let percent = download.bytes_received() * 100 / total;
            if percent >= last_percent + 10 {
                tlog!("  {}%", percent);
                last_percent = percent;
            }
        }
        tlog!("Download complete");
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_custom_timeout() {
        let device = try_device!(
            MtpDevice::builder()
                .timeout(Duration::from_secs(60))
                .open_first()
                .await,
            "open with timeout"
        );
        tlog!("Opened with 60s timeout: {}", device.device_info().model);
        device.close().await.expect("close failed");
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_ptp_device() {
        let device = try_device!(crate::open_test_ptp().await, "open PTP device");
        let info = try_device!(device.get_device_info().await, "get device info");
        tlog!("PTP Device: {} {}", info.manufacturer, info.model);

        let session = try_device!(device.open_session().await, "open session");
        let storage_ids = try_device!(session.get_storage_ids().await, "get storage IDs");
        tlog!("Storage IDs: {:?}", storage_ids);
        session.close().await.expect("close failed");
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_refresh_storage() {
        let device = try_device!(crate::open_test_mtp().await, "open device");
        let mut storages = try_device!(device.storages().await, "get storages");
        let storage = first_storage!(mut storages);

        let before = storage.info().free_space;
        try_device!(storage.refresh().await, "refresh storage");
        let after = storage.info().free_space;
        tlog!("Free space: {} -> {} bytes", before, after);
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_cancel_download_then_reuse_session() {
        // A cancel has two valid outcomes on real hardware:
        //  - the session stays healthy (most devices, and warm Samsung sessions):
        //    listing and a second download work immediately, or
        //  - a large in-flight backlog wedges the device (Samsung, #18): the
        //    library issues a USB DEVICE_RESET and returns `Error::DeviceReset`,
        //    and the caller must reopen (no physical replug).
        // Phase 1 runs the cancel and reports whether the device wedged.
        let wedged = {
            let device = try_device!(crate::open_test_mtp().await, "open device");
            let storages = try_device!(device.storages().await, "get storages");
            let storage = first_storage!(storages);

            tlog!("Searching for file (100KB-10MB) to cancel...");
            let Some((handle, file_size, file_name)) =
                find_suitable_file(storage, 100_000, 10_000_000).await
            else {
                tlog!("No suitable file found, skipping");
                return;
            };
            tlog!("Starting download of {} ({} bytes)", file_name, file_size);

            let mut download = try_device!(
                storage.download(handle, ByteRange::Full).await,
                "start download"
            );

            // Read just one chunk, then cancel
            let chunk = download.next_chunk().await.expect("expected a chunk");
            let bytes = chunk.expect("chunk error");
            tlog!(
                "Read {} bytes ({:.1}%), now cancelling...",
                bytes.len(),
                download.progress() * 100.0
            );

            let cancel_result = download.cancel(std::time::Duration::from_millis(300)).await;
            // Drop the download to release the session operation lock
            drop(download);

            match cancel_result {
                Ok(()) => {
                    tlog!("Cancel succeeded");

                    // Prove the session is still healthy by doing another operation
                    let objects =
                        try_device!(storage.list_objects(None).await, "list root after cancel");
                    tlog!(
                        "Session healthy: listed {} root objects after cancel",
                        objects.len()
                    );
                    assert!(!objects.is_empty());

                    // Do a second download to prove streaming still works
                    let mut download2 = try_device!(
                        storage.download(handle, ByteRange::Full).await,
                        "second download"
                    );
                    let mut total = 0u64;
                    while let Some(result) = download2.next_chunk().await {
                        total += result.expect("download2 error").len() as u64;
                    }
                    assert_eq!(total, file_size);
                    tlog!(
                        "Second full download succeeded ({} bytes). Cancel test PASSED",
                        total
                    );
                    false
                }
                Err(mtp_rs::Error::DeviceReset) => {
                    // The cancel wedged the device and the library auto-reset it
                    // (#18). The session is gone; Phase 2 verifies reopen works.
                    tlog!("Cancel wedged the device; library auto-reset it (#18). Verifying reopen...");
                    true
                }
                Err(e) => panic!("cancel returned an unexpected error: {e:?}"),
            }
        };

        if wedged {
            // Contract (design C): the library detected the wedge, reset the
            // transport to un-stick it, and returned DeviceReset. Reopening is
            // the caller's job and must be QUIET: post-reset the device needs a
            // beat with no traffic to finish tearing the old session down, so
            // wait, then reopen with idle-spaced retries (no hammering). This is
            // the documented consumer recovery, exercised here end-to-end.
            const QUIET: std::time::Duration = std::time::Duration::from_secs(3);
            const ATTEMPTS: u32 = 10;

            let mut recovered = false;
            for attempt in 1..=ATTEMPTS {
                tokio::time::sleep(QUIET).await;
                let objects = async {
                    let device = crate::open_test_mtp().await.ok()?;
                    let storages = device.storages().await.ok()?;
                    storages.first()?.list_objects(None).await.ok()
                }
                .await;
                match objects {
                    Some(objects) => {
                        tlog!(
                            "Recovered via reset + quiet reopen (attempt {attempt}): listed {} root objects, no replug. Cancel test PASSED",
                            objects.len()
                        );
                        assert!(!objects.is_empty());
                        recovered = true;
                        break;
                    }
                    None => tlog!(
                        "Reopen attempt {attempt}/{ATTEMPTS} not ready yet, waiting quietly..."
                    ),
                }
            }
            assert!(
                recovered,
                "device did not recover after reset within {ATTEMPTS} quiet reopen attempts; it may need a physical replug"
            );
        }
    }

    /// Test whether a session poisoned by a mid-stream drop can be recovered.
    /// This intentionally corrupts the session by dropping without cancel/drain,
    /// then tries a plain reopen and a transport-level `reset_device()`.
    ///
    /// **Opt-in only: this test can wedge the device.** Some camera firmware
    /// (Panasonic Lumix DMC-TZ61, #12) gets so stuck by a mid-stream drop that
    /// *no* software recovery reaches it: a reopen desyncs, and even the
    /// session-less SIC reset times out (libgphoto2 times out on it too). The
    /// only cure is physically unplugging and replugging the USB cable. On such
    /// a device this test leaves the session poisoned, which cascades into the
    /// rest of the suite. It is therefore excluded from default runs and only
    /// runs when `MTP_RUN_DROP_RECOVERY=1` is set, so you opt in knowing you may
    /// have to replug afterward.
    ///
    /// Run it with `--release` (the debug_assert in `ReceiveStream::Drop` is the
    /// exact scenario under test) and the opt-in flag:
    /// ```sh
    /// MTP_RUN_DROP_RECOVERY=1 cargo test --release --test integration test_drop_mid_stream -- --ignored --nocapture --test-threads=1
    /// ```
    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_drop_mid_stream_then_software_reconnect() {
        // Opt-in: this test can wedge the device past software recovery on some
        // firmware (see the doc comment), so it stays out of default runs.
        if !env_enabled("MTP_RUN_DROP_RECOVERY") {
            tlog!("SKIPPING: set MTP_RUN_DROP_RECOVERY=1 to run (can wedge the device until a USB replug)");
            return;
        }

        // This test intentionally drops mid-stream, which fires the debug_assert
        // in ReceiveStream::Drop. Skip in debug builds to avoid panicking.
        if cfg!(debug_assertions) {
            tlog!("SKIPPING: requires --release (debug_assert would panic)");
            return;
        }

        // Phase 1: Open device, start download, drop everything mid-stream.
        {
            let device = try_device!(crate::open_test_mtp().await, "open device");
            let storages = try_device!(device.storages().await, "get storages");
            let storage = first_storage!(storages);

            tlog!("Searching for file (100KB-10MB)...");
            let Some((handle, _file_size, file_name)) =
                find_suitable_file(storage, 100_000, 10_000_000).await
            else {
                tlog!("No suitable file found, skipping");
                return;
            };
            tlog!("Starting download of {} ({} bytes)", file_name, _file_size);

            let mut download = try_device!(
                storage.download(handle, ByteRange::Full).await,
                "start download"
            );

            // Read one chunk
            let chunk = download.next_chunk().await.expect("expected a chunk");
            let bytes = chunk.expect("chunk error");
            tlog!(
                "Read {} bytes ({:.1}%), now dropping without cancel...",
                bytes.len(),
                download.progress() * 100.0
            );

            // download drops here; in release mode the debug_assert is a no-op,
            // so Drop just releases the session lock and USB interface normally.
            // The USB pipe is left with stale data (intentionally).
        }
        tlog!("Dropped mid-stream (no cancel, no drain)");

        // Phase 2: Try a plain software reconnect (close + reopen the handle).
        // This is informative, not asserted: on Android a reopen often
        // recovers, but on PTP cameras (Panasonic Lumix DMC-TZ61, #12) it does
        // NOT: the device still has the abandoned transaction's data queued,
        // so the fresh session reads it as a desync ("expected Response
        // container type (3), got ..."). Either outcome is fine to observe.
        tlog!("Attempting software reconnect (plain reopen)...");
        match crate::open_test_mtp().await {
            Ok(device2) => {
                tlog!("Reconnected: {}", device2.device_info().model);
                match device2.storages().await {
                    Ok(storages2) => {
                        let storage2 = first_storage!(storages2);
                        match storage2.list_objects(None).await {
                            Ok(objects) => {
                                tlog!("Plain reopen WORKS: listed {} root objects", objects.len());
                            }
                            Err(e) => {
                                tlog!("Plain reopen FAILED at list: {:?}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tlog!("Plain reopen FAILED at storages: {:?}", e);
                    }
                }
            }
            Err(e) => {
                tlog!("Plain reopen FAILED at open: {:?}", e);
            }
        }

        // Phase 3: Attempt recovery via a transport-level reset.
        //
        // When a plain reopen can't clear the device's stuck transaction, the
        // session-less SIC DEVICE_RESET usually can (PtpDevice opens without a
        // session, so it works precisely when MtpDevice::open can't). On most
        // devices this cleans up the poisoned session so the rest of the suite
        // runs. But on firmware that wedges hard on a mid-stream drop (Lumix
        // DMC-TZ61, #12), even this reset times out: at that point only a
        // physical USB replug recovers the device, which is why the test is
        // opt-in.
        let mut recovered = false;
        tlog!("Recovering with a transport-level reset...");
        match crate::open_test_ptp().await {
            Ok(ptp) => match ptp.reset_device().await {
                Ok(()) => match ptp.get_device_info().await {
                    Ok(info) => {
                        recovered = true;
                        tlog!("Reset recovered the device: {}", info.model);
                    }
                    Err(e) => tlog!(
                        "Reset sent; device not answering yet (it may need a moment): {:?}",
                        e
                    ),
                },
                Err(e) => tlog!("Reset failed: {:?}", e),
            },
            Err(e) => tlog!("Could not open device to reset: {:?}", e),
        }

        if !recovered {
            tlog!(
                "Device is still wedged: this firmware can't recover a mid-stream drop in \
                 software. Unplug and replug the USB cable to reset it; later tests in this \
                 run will fail until you do."
            );
        }

        // Some cameras need a beat of idle time between USB sessions (the Lumix
        // returns Timeout on an immediately-following open). Give the device
        // breathing room so the next test starts from a clean slate.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_streaming_download() {
        let device = try_device!(crate::open_test_mtp().await, "open device");
        let storages = try_device!(device.storages().await, "get storages");
        let storage = first_storage!(storages);

        tlog!("Searching for file (100KB-5MB)...");
        let Some((handle, file_size, file_name)) =
            find_suitable_file(storage, 100_000, 5_000_000).await
        else {
            tlog!("No suitable file found, skipping");
            return;
        };
        tlog!("Streaming {} ({} bytes)", file_name, file_size);

        let mut download = try_device!(
            storage.download(handle, ByteRange::Full).await,
            "start download"
        );
        assert_eq!(download.size(), file_size);

        let mut total_received = 0u64;
        let mut chunk_count = 0u64;

        while let Some(result) = download.next_chunk().await {
            let chunk = result.expect("download error");
            total_received += chunk.len() as u64;
            chunk_count += 1;
        }

        tlog!(
            "Received {} bytes in {} chunks",
            total_received,
            chunk_count
        );
        assert_eq!(total_received, file_size);
    }

    /// Windowed download against a real device: read a large file via
    /// `download_windowed`, checksum it against a full `download_stream` of the
    /// same file, AND prove the PTP session is free between windows by running a
    /// folder listing in the middle of the windowed read.
    ///
    /// This is the headline property the windowed API exists for: a held-open
    /// `download_stream` would block any other op until it finished or was
    /// cancelled (and cancelling a multi-GB read costs ~35s to drain); the
    /// windowed read frees the session between every window, so the listing here
    /// just works.
    ///
    /// Run it (with the phone connected and unlocked):
    ///
    /// ```sh
    /// cargo test -p mtp-rs --test integration -- --ignored windowed
    /// ```
    ///
    /// Set `MTP_TEST_READFILE=/DCIM/.../big.mp4` to pin a specific large file;
    /// otherwise it searches for one in the 5–200 MB range.
    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_windowed_download_matches_stream_and_frees_session() {
        let device = try_device!(crate::open_test_mtp().await, "open device");
        let storages = try_device!(device.storages().await, "get storages");
        let storage = first_storage!(storages);

        // A larger file than the streaming test, so the multi-window path and the
        // ~80ms/window cadence are meaningfully exercised on real hardware.
        tlog!("Searching for file (5MB-200MB)...");
        let Some((handle, file_size, file_name)) =
            find_suitable_file(storage, 5_000_000, 200_000_000).await
        else {
            tlog!("No suitable file found, skipping");
            return;
        };
        tlog!("Windowed-reading {} ({} bytes)", file_name, file_size);

        // Reference: a plain full streaming download, reduced to a 64-bit FNV-1a
        // checksum so we don't hold the whole file in RAM twice.
        let mut reference = try_device!(
            storage.download(handle, ByteRange::Full).await,
            "start stream"
        );
        assert_eq!(reference.size(), file_size);
        let mut ref_hash = FNV_OFFSET;
        let mut ref_len = 0u64;
        while let Some(result) = reference.next_chunk().await {
            let bytes = result.expect("stream chunk error");
            ref_len += bytes.len() as u64;
            ref_hash = fnv1a_update(ref_hash, &bytes);
        }
        assert_eq!(ref_len, file_size, "full stream length mismatch");
        drop(reference);

        // Windowed read with the default 8 MiB window, listing the storage root
        // between windows to prove the session is free.
        let mut windowed = try_device!(
            storage.download_windowed_default(handle).await,
            "start windowed"
        );
        assert_eq!(
            windowed.size(),
            file_size,
            "windowed size() must report the full object size"
        );
        let mut win_hash = FNV_OFFSET;
        let mut win_len = 0u64;
        let mut window_count = 0u64;
        let mut listings_between = 0u64;
        while let Some(window) = windowed.next_window().await {
            let bytes = match window {
                Ok(bytes) => bytes,
                // Device advertises neither GetPartialObject64 nor the 32-bit
                // GetPartialObject: windowed reads can't work here. Skip cleanly
                // rather than fail. (A 32-bit-only camera like the Lumix DMC-TZ61
                // uses the fallback and does NOT hit this.)
                Err(mtp_rs::Error::Unsupported) => {
                    tlog!("SKIPPING: device supports no partial-object read op");
                    return;
                }
                Err(e) => panic!("window error: {e:?}"),
            };
            win_len += bytes.len() as u64;
            win_hash = fnv1a_update(win_hash, &bytes);
            window_count += 1;

            // BETWEEN windows: the session is free. Run a real listing on the
            // same session. (Do it after the first window so there's a read in
            // flight conceptually, but only a few times to keep the test quick.)
            if listings_between < 3 {
                let listed = try_device!(
                    storage.list_objects(None).await,
                    "list root between windows"
                );
                listings_between += 1;
                tlog!(
                    "Between windows: listed {} root object(s). Session is free.",
                    listed.len()
                );
            }
        }

        tlog!(
            "Windowed: {} bytes in {} windows, {} listings interleaved.",
            win_len,
            window_count,
            listings_between
        );
        assert_eq!(win_len, file_size, "windowed length mismatch");
        assert_eq!(
            win_hash, ref_hash,
            "windowed download checksum must equal the full stream's"
        );
        assert!(
            listings_between > 0,
            "at least one listing must have run between windows"
        );
    }
}

/// FNV-1a 64-bit offset basis, for the integration checksum.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit incremental update. Lets the windowed integration test compare
/// a multi-hundred-MB download against a full stream without buffering it twice.
fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// SplitMix64, the pattern generator behind the >4 GiB round trip.
///
/// Every 8-byte word of the test payload is `splitmix64(byte_offset / 8)`, so
/// the expected bytes at any offset can be regenerated without keeping the file
/// anywhere. Position-dependent on purpose: a constant fill or a repeated block
/// would survive an offset shift, a dropped chunk, or a duplicated one.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fill `buf` with the [`splitmix64`] pattern for the byte range starting at
/// `start_offset`. Word-aligned or not, the bytes only depend on their absolute
/// offset, so any chunking of the stream produces the same file.
fn fill_pattern(buf: &mut [u8], start_offset: u64) {
    let mut pos = 0usize;
    while pos < buf.len() {
        let abs = start_offset + pos as u64;
        let within = (abs % 8) as usize;
        let word = splitmix64(abs / 8).to_le_bytes();
        let n = (8 - within).min(buf.len() - pos);
        buf[pos..pos + n].copy_from_slice(&word[within..within + n]);
        pos += n;
    }
}

/// A generated upload source: yields the [`fill_pattern`] bytes chunk by chunk
/// and never holds more than one chunk, so a multi-GB upload costs no disk and
/// a megabyte of RAM. Hand-rolled rather than `stream::unfold` because
/// `Storage::upload` wants `Unpin`.
struct PatternStream {
    offset: u64,
    total: u64,
    chunk_size: usize,
}

impl futures::Stream for PatternStream {
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.offset >= self.total {
            return std::task::Poll::Ready(None);
        }
        let n = (self.total - self.offset).min(self.chunk_size as u64) as usize;
        let mut buf = vec![0u8; n];
        fill_pattern(&mut buf, self.offset);
        self.offset += n as u64;
        std::task::Poll::Ready(Some(Ok(bytes::Bytes::from(buf))))
    }
}

// Camera control tests disabled - need PtpSession device property methods.

/// Destructive tests - these write to the device.
mod destructive {
    use super::*;
    use bytes::Bytes;
    use mtp_rs::mtp::{MtpDevice, NewObjectInfo};
    use mtp_rs::Error;

    /// Helper to get device, storage, and a writable folder handle.
    ///
    /// Walks a priority list of common folder names (or the
    /// `MTP_TEST_FOLDER` override). Logs which folder was selected so test
    /// output is unambiguous on devices with non-standard layouts.
    ///
    /// Returns `None` after logging the reason when the test should skip:
    /// no device, device doesn't advertise upload support (read-only
    /// devices like PTP cameras), or no writable folder found.
    async fn setup_with_writable_folder() -> Option<(MtpDevice, mtp_rs::mtp::Storage, ObjectHandle)>
    {
        let device = match crate::open_test_mtp().await {
            Ok(d) => d,
            Err(e) => {
                tlog!("SKIPPING: open device - {:?}", e);
                return None;
            }
        };
        if !device.supports_upload() {
            tlog!("Device doesn't support upload (no SendObjectInfo/SendObject), skipping");
            return None;
        }
        let storages = match device.storages().await {
            Ok(s) => s,
            Err(e) => {
                tlog!("SKIPPING: get storages - {:?}", e);
                return None;
            }
        };
        let storage = storages.into_iter().next()?;
        let Some((handle, name)) = find_writable_folder(&storage).await else {
            tlog!("No writable folder found, skipping (set MTP_TEST_FOLDER to override)");
            return None;
        };
        tlog!("Using writable folder: {}", name);
        Some((device, storage, handle))
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_upload_download_delete() {
        let Some((_device, storage, folder_handle)) = setup_with_writable_folder().await else {
            // setup_with_writable_folder() already logged the skip reason
            return;
        };

        let content = format!("Test file at {:?}", std::time::SystemTime::now());
        let content_bytes = content.as_bytes();

        tlog!("Uploading {} bytes...", content_bytes.len());
        let info = NewObjectInfo::file("mtp-rs-test.txt", content_bytes.len() as u64);
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(
            content_bytes.to_vec(),
        ))]);
        let handle = storage
            .upload(Some(folder_handle), info, Box::pin(stream))
            .await
            .expect("upload failed");

        // Verify
        let obj_info = storage
            .get_object_info(handle)
            .await
            .expect("get info failed");
        assert_eq!(obj_info.filename, "mtp-rs-test.txt");
        assert_eq!(obj_info.size, content_bytes.len() as u64);

        // Download and verify content
        let downloaded = storage
            .download_to_vec(handle)
            .await
            .expect("download failed");
        assert_eq!(downloaded, content_bytes);
        tlog!("Content verified");

        // Delete and verify
        storage.delete(handle).await.expect("delete failed");
        let result = storage.get_object_info(handle).await;
        assert!(matches!(result, Err(Error::StaleHandle | Error::NotFound)));
        tlog!("Upload/download/delete PASSED");
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_create_delete_folder() {
        let Some((_device, storage, folder_handle)) = setup_with_writable_folder().await else {
            // setup_with_writable_folder() already logged the skip reason
            return;
        };

        let folder_name = format!("mtp-rs-test-{}", std::process::id());
        tlog!("Creating folder: {}", folder_name);

        let handle = storage
            .create_folder(Some(folder_handle), &folder_name)
            .await
            .expect("create failed");

        let info = storage
            .get_object_info(handle)
            .await
            .expect("get info failed");
        assert!(info.is_folder());
        assert_eq!(info.filename, folder_name);

        storage.delete(handle).await.expect("delete failed");
        tlog!("Create/delete folder PASSED");
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_rename_file() {
        let device = try_device!(crate::open_test_mtp().await, "open device");

        if !device.supports_rename() {
            tlog!("Device doesn't support rename, skipping");
            return;
        }

        if !device.supports_upload() {
            tlog!("Device doesn't support upload (no SendObjectInfo/SendObject), skipping");
            return;
        }

        let storages = try_device!(device.storages().await, "get storages");
        let storage = first_storage!(storages);
        let Some((folder_handle, folder_name)) = find_writable_folder(storage).await else {
            tlog!("No writable folder found, skipping (set MTP_TEST_FOLDER to override)");
            return;
        };
        tlog!("Using writable folder: {}", folder_name);

        let original = format!("mtp-rs-rename-{}.txt", std::process::id());
        let renamed = format!("mtp-rs-renamed-{}.txt", std::process::id());
        let content = b"rename test";

        let info = NewObjectInfo::file(&original, content.len() as u64);
        let stream =
            futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(content.to_vec()))]);
        let handle = storage
            .upload(Some(folder_handle), info, Box::pin(stream))
            .await
            .expect("upload failed");

        tlog!("Renaming {} -> {}", original, renamed);
        match storage.rename(handle, &renamed).await {
            Ok(()) => {
                let info = storage
                    .get_object_info(handle)
                    .await
                    .expect("get info failed");
                assert_eq!(info.filename, renamed);
                tlog!("Rename verified");
            }
            Err(Error::Unsupported) => {
                tlog!("Rename not actually supported (device lied)");
            }
            Err(e) => {
                storage.delete(handle).await.ok();
                panic!("Rename failed: {:?}", e);
            }
        }

        storage.delete(handle).await.expect("cleanup failed");
        tlog!("Rename test PASSED");
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_streaming_upload() {
        let Some((_device, storage, folder_handle)) = setup_with_writable_folder().await else {
            // setup_with_writable_folder() already logged the skip reason
            return;
        };

        let chunk_size = 64 * 1024;
        let num_chunks = 48; // 3 MB -- exercises multi-batch streaming
        let total_size = chunk_size * num_chunks;

        tlog!("Uploading {} bytes in {} chunks", total_size, num_chunks);

        let chunks: Vec<Result<Bytes, std::io::Error>> = (0..num_chunks)
            .map(|i| Ok(Bytes::from(vec![i as u8; chunk_size])))
            .collect();

        let filename = format!("mtp-rs-stream-{}.bin", std::process::id());
        let info = NewObjectInfo::file(&filename, total_size as u64);
        let handle = storage
            .upload(Some(folder_handle), info, futures::stream::iter(chunks))
            .await
            .expect("upload failed");

        // Verify
        let obj_info = storage
            .get_object_info(handle)
            .await
            .expect("get info failed");
        assert_eq!(obj_info.size, total_size as u64);

        let downloaded = storage
            .download_to_vec(handle)
            .await
            .expect("download failed");
        for i in 0..num_chunks {
            let start = i * chunk_size;
            assert!(downloaded[start..start + chunk_size]
                .iter()
                .all(|&b| b == i as u8));
        }

        storage.delete(handle).await.expect("cleanup failed");
        tlog!("Streaming upload PASSED");
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_streaming_copy() {
        let Some((_device, storage, folder_handle)) = setup_with_writable_folder().await else {
            // setup_with_writable_folder() already logged the skip reason
            return;
        };

        // Find a file to copy
        let objects = storage
            .list_objects(Some(folder_handle))
            .await
            .unwrap_or_default();
        let Some(source) = objects
            .iter()
            .find(|o| o.is_file() && o.size > 50_000 && o.size < 500_000)
        else {
            tlog!("No suitable source file (50KB-500KB), skipping");
            return;
        };

        let source_handle = source.handle;
        let source_size = source.size;
        tlog!("Copying {} ({} bytes)", source.filename, source_size);

        // Download
        let download = storage
            .download(source_handle, ByteRange::Full)
            .await
            .expect("download failed");
        let data = download.collect().await.expect("collect failed");

        // Upload copy
        let dest_name = format!("mtp-rs-copy-{}.bin", std::process::id());
        let info = NewObjectInfo::file(&dest_name, source_size);
        let stream =
            futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(data.clone()))]);
        let dest_handle = storage
            .upload(Some(folder_handle), info, stream)
            .await
            .expect("upload failed");

        // Verify
        let copy_data = storage
            .download_to_vec(dest_handle)
            .await
            .expect("download copy failed");
        assert_eq!(copy_data, data);

        storage.delete(dest_handle).await.expect("cleanup failed");
        tlog!("Streaming copy PASSED");
    }

    /// Round-trips an object larger than 4 GiB against real hardware.
    ///
    /// What it guards: an object past 4 GiB doesn't fit a 32-bit PTP
    /// `ContainerLength`, so responders send the `0xFFFFFFFF` sentinel instead
    /// (MTP 1.1 appendix H.1) and the real length comes from `ObjectSize`. The
    /// `receive_stream_*` unit tests pin that contract against a synthetic
    /// transport; only this test proves a real responder is read correctly.
    /// Read the sentinel as a literal byte count and the download dies with a
    /// bogus container type partway in.
    ///
    /// Cost: about 25 seconds each way on a Pixel 9 Pro XL over SuperSpeed USB,
    /// plus a few seconds of pattern generation. Needs the payload size (4 GiB +
    /// 64 MiB by default) free on the device, and the test skips if the storage
    /// reports less than that plus a 512 MiB margin. Nothing touches local disk:
    /// the payload is generated on the fly and verified against a regenerated
    /// copy as chunks arrive.
    ///
    /// Double-gated: `#[ignore]` plus `MTP_TEST_BIG_FILE=1`, because the
    /// documented way to run this file is `-- --ignored`, and this test has no
    /// business joining every hardware sweep. Size override:
    /// `MTP_TEST_BIG_FILE_SIZE` in bytes.
    ///
    /// Warning: if it fails mid-transfer, the device may be wedged badly enough
    /// that `mtp-rs reset` won't clear it and it needs a physical replug before
    /// anything can talk to it again. Cleanup runs on the panic path, but it
    /// talks to the same wedged device, so it logs `couldn't delete ...` and
    /// gives up rather than hanging. Expect to replug and then remove the
    /// multi-GB payload by hand (`mtp-rs rm --yes /Download/mtp-rs-bigfile-*`).
    #[tokio::test]
    #[ignore]
    #[serial]
    async fn test_big_file_over_4gib_round_trip() {
        use futures::FutureExt;
        use std::panic::AssertUnwindSafe;
        use std::sync::{Arc, Mutex};

        const FOUR_GIB: u64 = 4 * 1024 * 1024 * 1024;
        /// Headroom we leave on the device on top of the payload.
        const FREE_SPACE_MARGIN: u64 = 512 * 1024 * 1024;
        /// Upload chunk size. Big enough to keep the USB pipe busy, small
        /// enough that one chunk is a rounding error in RAM.
        const CHUNK_SIZE: usize = 1024 * 1024;

        if !env_enabled("MTP_TEST_BIG_FILE") {
            tlog!("SKIPPING test_big_file_over_4gib_round_trip: set MTP_TEST_BIG_FILE=1 to run it");
            tlog!("  It writes 4 GiB + 64 MiB to the device and takes about a minute.");
            return;
        }

        let size = std::env::var("MTP_TEST_BIG_FILE_SIZE")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&s| s > 0)
            .unwrap_or(FOUR_GIB + 64 * 1024 * 1024);
        if size <= FOUR_GIB {
            tlog!(
                "WARNING: MTP_TEST_BIG_FILE_SIZE={} is not past 4 GiB, so the 0xFFFFFFFF",
                size
            );
            tlog!("  container-length sentinel will NOT be exercised. This run proves nothing");
            tlog!("  about the >4 GiB path.");
        }

        let Some((_device, mut storage, folder_handle)) = setup_with_writable_folder().await else {
            // setup_with_writable_folder() already logged the skip reason
            return;
        };

        if let Err(e) = storage.refresh().await {
            tlog!("SKIPPING: couldn't refresh storage info - {:?}", e);
            return;
        }
        let free = storage.info().free_space;
        let needed = size.saturating_add(FREE_SPACE_MARGIN);
        if free < needed {
            tlog!(
                "SKIPPING: device has {:.2} GB free, needs {:.2} GB (payload + 512 MiB margin)",
                free as f64 / 1e9,
                needed as f64 / 1e9
            );
            return;
        }
        tlog!(
            "Payload {} bytes ({:.2} GB); device has {:.2} GB free",
            size,
            size as f64 / 1e9,
            free as f64 / 1e9
        );

        let filename = format!("mtp-rs-bigfile-{}.bin", std::process::id());
        // Set as soon as the object exists on the device, so cleanup below can
        // remove it whether the body finished, returned early, or panicked.
        let uploaded: Arc<Mutex<Option<ObjectHandle>>> = Arc::new(Mutex::new(None));

        let body = {
            let uploaded = Arc::clone(&uploaded);
            let storage = &storage;
            let filename = filename.as_str();
            async move {
                tlog!("Uploading {} from a generated stream...", filename);
                let upload_start = Instant::now();
                let info = NewObjectInfo::file(filename, size);
                let source = PatternStream {
                    offset: 0,
                    total: size,
                    chunk_size: CHUNK_SIZE,
                };
                let handle = match storage.upload(Some(folder_handle), info, source).await {
                    Ok(h) => h,
                    Err(e) => {
                        // A failed data phase can still leave a partial object;
                        // hand it to the cleanup path before panicking.
                        if let Some(partial) = e.partial {
                            *uploaded.lock().unwrap() = Some(partial);
                        }
                        panic!("upload failed: {e:?}");
                    }
                };
                *uploaded.lock().unwrap() = Some(handle);
                let upload_secs = upload_start.elapsed().as_secs_f64();
                tlog!(
                    "Uploaded in {:.1}s ({:.1} MB/s)",
                    upload_secs,
                    size as f64 / 1e6 / upload_secs
                );

                let obj_info = storage
                    .get_object_info(handle)
                    .await
                    .expect("get info failed");
                assert_eq!(obj_info.filename, filename);
                assert_eq!(
                    obj_info.size, size,
                    "device reports a different size than we sent"
                );

                tlog!("Downloading and verifying against the regenerated pattern...");
                let download_start = Instant::now();
                let mut download = storage
                    .download(handle, ByteRange::Full)
                    .await
                    .expect("start download failed");
                assert_eq!(
                    download.size(),
                    size,
                    "download must report the full object size, not a 32-bit truncation"
                );

                let mut received = 0u64;
                let mut sent_hash = FNV_OFFSET;
                let mut recv_hash = FNV_OFFSET;
                let mut expected = Vec::new();
                let mut last_logged_percent = 0u64;
                while let Some(result) = download.next_chunk().await {
                    let bytes = result.expect("download chunk error");
                    assert!(
                        received + bytes.len() as u64 <= size,
                        "device sent more than {size} bytes"
                    );
                    expected.resize(bytes.len(), 0);
                    fill_pattern(&mut expected, received);
                    // Byte-exact, so a 12-byte injection or a dropped chunk
                    // fails here with the offset instead of only at the end.
                    assert!(
                        expected[..] == bytes[..],
                        "payload mismatch in the chunk starting at offset {received}"
                    );
                    sent_hash = fnv1a_update(sent_hash, &expected);
                    recv_hash = fnv1a_update(recv_hash, &bytes);
                    received += bytes.len() as u64;

                    let percent = received * 100 / size;
                    if percent >= last_logged_percent + 10 {
                        tlog!("  {}%", percent);
                        last_logged_percent = percent;
                    }
                }
                let download_secs = download_start.elapsed().as_secs_f64();

                // Catches a truncation, which per-chunk comparison alone can't.
                assert_eq!(received, size, "downloaded byte count mismatch");
                assert_eq!(recv_hash, sent_hash, "payload checksum mismatch");
                tlog!(
                    "Downloaded {} bytes in {:.1}s ({:.1} MB/s), FNV-1a {:#018x}, byte-identical",
                    received,
                    download_secs,
                    size as f64 / 1e6 / download_secs,
                    recv_hash
                );
            }
        };

        let outcome = AssertUnwindSafe(body).catch_unwind().await;

        // Cleanup runs on both paths: a panicking assertion must not strand
        // 4+ GB on someone's phone.
        let handle = uploaded.lock().unwrap().take();
        if let Some(handle) = handle {
            match storage.delete(handle).await {
                Ok(()) => tlog!("Deleted {}", filename),
                Err(e) => tlog!("WARNING: couldn't delete {}: {:?}", filename, e),
            }
        }

        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
        tlog!("Big-file (>4 GiB) round trip PASSED");
    }
}
