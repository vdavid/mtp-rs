//! Paced uploads (#62) against the virtual device's model of Android's edit operations: the
//! host rules that keep a real phone from wedging, tested without hardware.
//!
//! The two rules: never send a window the phone refuses before reading it (an object not open
//! for editing, or an offset past the edit's end), and on any stop truncate back to what the
//! phone acknowledged and end the edit. The device model wedges on the first and keeps a stopped
//! window's bytes past the edit for the second, so a host that broke either fails here.

use super::config::{VirtualDeviceConfig, VirtualStorageConfig};
use super::registry::operation_count;
use crate::mtp::{MtpDevice, NewObjectInfo, ObjectHandle, Storage, UploadPacing};
use crate::ptp::OperationCode;
use crate::virtual_device::{virtual_device_wedged, virtual_open_edits};
use crate::Error;
use bytes::Bytes;
use futures::StreamExt;
use std::ops::ControlFlow;
use std::path::Path;
use std::time::Duration;

const MIB: usize = 1024 * 1024;

fn config(dir: &Path, serial: &str) -> VirtualDeviceConfig {
    VirtualDeviceConfig {
        serial: serial.into(),
        storages: vec![VirtualStorageConfig {
            description: "Internal Storage".into(),
            capacity: 1024 * 1024 * 1024,
            backing_dir: dir.to_path_buf(),
            read_only: false,
        }],
        event_poll_interval: Duration::ZERO,
        watch_backing_dirs: false,
        ..Default::default()
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

/// `data` as a stream of 256 KiB chunks, like a file read.
fn chunks(data: &[u8]) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Unpin + Send {
    let chunks: Vec<_> = data
        .chunks(256 * 1024)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    futures::stream::iter(chunks)
}

async fn open(dir: &Path, serial: &str) -> (MtpDevice, Storage) {
    let device = MtpDevice::builder()
        .open_virtual(config(dir, serial))
        .await
        .unwrap();
    let storage = device.storages().await.unwrap().remove(0);
    (device, storage)
}

fn count(serial: &str, op: OperationCode) -> u32 {
    operation_count(serial, op).unwrap()
}

/// The device is healthy: not wedged, no edit left open, and it answers.
async fn assert_clean(storage: &Storage, serial: &str) {
    assert_eq!(virtual_device_wedged(serial), Some(false), "wedged");
    storage
        .list_objects(None)
        .await
        .expect("the device must answer");
    assert_eq!(virtual_open_edits(serial), Some(0), "an edit was left open");
}

/// Poll `upload` until `done()` holds, asserting it never finishes on the way. Feeding a chunk
/// through the transfer's channel can return `Pending` once on its way, so a test that wants a
/// state between or inside windows waits for it instead of counting polls.
async fn poll_until<F: std::future::Future>(
    mut upload: std::pin::Pin<&mut F>,
    mut done: impl FnMut() -> bool,
) {
    for _ in 0..1000 {
        assert!(futures::poll!(upload.as_mut()).is_pending(), "it finished");
        if done() {
            return;
        }
    }
    panic!("never got there");
}

fn continue_(_: crate::mtp::Progress) -> ControlFlow<()> {
    ControlFlow::Continue(())
}

#[tokio::test]
async fn a_windowed_upload_arrives_whole_in_the_windows_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-whole";
    let (_device, storage) = open(dir.path(), serial).await;
    assert!(_device.capabilities().supports_windowed_upload);
    let data = pattern(3 * MIB + 12_345);

    let handle = storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("whole.bin", data.len() as u64),
            chunks(&data),
            UploadPacing::FixedWindow(MIB as u32),
            continue_,
        )
        .await
        .unwrap();

    assert_eq!(std::fs::read(dir.path().join("whole.bin")).unwrap(), data);
    assert_eq!(storage.download_to_vec(handle).await.unwrap(), data);
    assert_eq!(count(serial, OperationCode::SendPartialObject), 4);
    assert_eq!(count(serial, OperationCode::BeginEditObject), 1);
    assert_eq!(count(serial, OperationCode::EndEditObject), 1);
    assert_eq!(count(serial, OperationCode::TruncateObject), 0);
    assert_clean(&storage, serial).await;
}

// --- The default pacing (#68): held open where that keeps the date, windows otherwise ---

/// A date far enough from now that the read-back can tell "kept" from "dated now".
fn old_date() -> crate::mtp::DateTime {
    crate::mtp::DateTime::new(2019, 6, 14, 10, 23, 0)
        .unwrap()
        .with_offset(crate::mtp::UtcOffset::UTC)
}

#[tokio::test]
async fn an_undated_upload_goes_in_windows_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-default";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(20 * MIB);

    storage
        .upload(
            None,
            NewObjectInfo::file("default.bin", data.len() as u64),
            chunks(&data),
        )
        .await
        .unwrap();

    assert_eq!(std::fs::read(dir.path().join("default.bin")).unwrap(), data);
    // 8 MiB first, then a window sized from the speed.
    assert!(count(serial, OperationCode::SendPartialObject) >= 2);
    assert_clean(&storage, serial).await;
}

/// A Pixel keeps a held-open upload's date, so dated uploads stay held open, and one read-back
/// learns that the device keeps dates.
#[tokio::test]
async fn a_dated_upload_stays_held_open_and_keeps_its_date_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-default-dated";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(3 * MIB);

    for name in ["kept-1.bin", "kept-2.bin", "kept-3.bin"] {
        let handle = storage
            .upload(
                None,
                NewObjectInfo::file(name, data.len() as u64).with_modified(old_date()),
                chunks(&data),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), data);
        let modified = storage.get_object_info(handle).await.unwrap().modified;
        assert_eq!(modified.map(|d| d.year), Some(2019), "{modified:?}");
    }
    assert_eq!(count(serial, OperationCode::SendPartialObject), 0);
    assert_eq!(count(serial, OperationCode::SendObject), 3);
    // One read-back for the first upload, then the three asked here.
    assert_eq!(count(serial, OperationCode::GetObjectInfo), 4);
}

/// A Galaxy S23 Ultra drops the sent date even held open. The first dated upload is held open
/// and read back; from then on dated uploads go in windows too, since holding them open buys
/// nothing.
#[tokio::test]
async fn a_device_that_drops_dates_gets_windows_after_the_first_dated_upload() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-drops-dates";
    let device = MtpDevice::builder()
        .open_virtual(VirtualDeviceConfig {
            keeps_upload_dates: false,
            ..config(dir.path(), serial)
        })
        .await
        .unwrap();
    let storage = device.storages().await.unwrap().remove(0);
    let data = pattern(3 * MIB);

    for name in ["first.bin", "second.bin"] {
        storage
            .upload(
                None,
                NewObjectInfo::file(name, data.len() as u64).with_modified(old_date()),
                chunks(&data),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), data);
    }
    // The first went out held open, the second in windows.
    assert_eq!(count(serial, OperationCode::BeginEditObject), 1);
    assert!(count(serial, OperationCode::SendPartialObject) >= 1);
    assert_eq!(count(serial, OperationCode::GetObjectInfo), 1);

    // The step-by-step upload follows the same rule.
    let upload = storage
        .start_upload(
            None,
            NewObjectInfo::file("third.bin", 10).with_modified(old_date()),
            UploadPacing::default(),
        )
        .await
        .unwrap();
    assert!(upload.is_windowed());
    let _ = upload.cancel().await;
    assert_clean(&storage, serial).await;
}

/// A date within a few days of now reads the same kept or dropped (zones put a zoneless date up
/// to a day off), so it teaches nothing: held open, and no read-back.
#[tokio::test]
async fn a_recent_date_stays_held_open_without_a_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-recent-date";
    let device = MtpDevice::builder()
        .open_virtual(VirtualDeviceConfig {
            keeps_upload_dates: false,
            ..config(dir.path(), serial)
        })
        .await
        .unwrap();
    let storage = device.storages().await.unwrap().remove(0);
    let data = pattern(2 * MIB);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let recent =
        crate::mtp::DateTime::from_unix_seconds(now - 3600, crate::mtp::UtcOffset::UTC).unwrap();

    for name in ["recent-1.bin", "recent-2.bin"] {
        storage
            .upload(
                None,
                NewObjectInfo::file(name, data.len() as u64).with_modified(recent),
                chunks(&data),
            )
            .await
            .unwrap();
    }
    assert_eq!(count(serial, OperationCode::SendPartialObject), 0);
    assert_eq!(count(serial, OperationCode::GetObjectInfo), 0);
}

#[tokio::test]
async fn a_step_by_step_upload_follows_the_default_rule() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "file-up-default";
    let (_device, storage) = open(dir.path(), serial).await;

    let dated = storage
        .start_upload(
            None,
            NewObjectInfo::file("dated.bin", 10).with_modified(old_date()),
            UploadPacing::default(),
        )
        .await
        .unwrap();
    assert!(!dated.is_windowed());
    let _ = dated.cancel().await;

    let undated = storage
        .start_upload(
            None,
            NewObjectInfo::file("undated.bin", 10),
            UploadPacing::default(),
        )
        .await
        .unwrap();
    assert!(undated.is_windowed());
    let _ = undated.cancel().await;
    assert_clean(&storage, serial).await;
}

/// An explicit pacing always wins over the date rule.
#[tokio::test]
async fn an_explicit_pacing_wins_over_the_date_rule() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-explicit";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(2 * MIB);

    storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("dated.bin", data.len() as u64).with_modified(old_date()),
            chunks(&data),
            UploadPacing::FixedWindow(MIB as u32),
            continue_,
        )
        .await
        .unwrap();
    assert_eq!(count(serial, OperationCode::SendPartialObject), 2);
    storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("undated.bin", data.len() as u64),
            chunks(&data),
            UploadPacing::HeldOpen,
            continue_,
        )
        .await
        .unwrap();
    assert_eq!(count(serial, OperationCode::SendPartialObject), 2);
    // No read-back on an explicit pacing.
    assert_eq!(count(serial, OperationCode::GetObjectInfo), 0);
}

#[tokio::test]
async fn a_device_default_pacing_puts_every_upload_in_windows() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-device-default";
    let device = MtpDevice::builder()
        .upload_pacing(UploadPacing::MaxWindowTime(
            crate::mtp::DEFAULT_UPLOAD_WINDOW_TIME,
        ))
        .open_virtual(config(dir.path(), serial))
        .await
        .unwrap();
    let storage = device.storages().await.unwrap().remove(0);
    let data = pattern(20 * MIB);

    storage
        .upload(
            None,
            NewObjectInfo::file("windowed.bin", data.len() as u64),
            chunks(&data),
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read(dir.path().join("windowed.bin")).unwrap(),
        data
    );
    // 8 MiB first, then a window sized from the speed.
    assert!(count(serial, OperationCode::SendPartialObject) >= 2);
    assert_clean(&storage, serial).await;
}

#[tokio::test]
async fn held_open_pacing_sends_one_send_object() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern(2 * MIB);
    let serial = "paced-up-held";
    let (_device, storage) = open(dir.path(), serial).await;
    storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("held.bin", data.len() as u64),
            chunks(&data),
            UploadPacing::HeldOpen,
            continue_,
        )
        .await
        .unwrap();
    assert_eq!(count(serial, OperationCode::SendPartialObject), 0);
    assert_eq!(std::fs::read(dir.path().join("held.bin")).unwrap(), data);
}

#[tokio::test]
async fn devices_without_the_edit_operations_upload_in_one_piece() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern(2 * MIB);
    for (serial, cfg) in [
        (
            "paced-up-camera",
            VirtualDeviceConfig {
                android: false,
                ..config(dir.path(), "paced-up-camera")
            },
        ),
        (
            "paced-up-noedit",
            VirtualDeviceConfig {
                supports_edit_objects: false,
                ..config(dir.path(), "paced-up-noedit")
            },
        ),
    ] {
        let device = MtpDevice::builder().open_virtual(cfg).await.unwrap();
        assert!(!device.capabilities().supports_windowed_upload, "{serial}");
        let storage = device.storages().await.unwrap().remove(0);
        let name = format!("{serial}.bin");
        storage
            .upload(
                None,
                NewObjectInfo::file(&name, data.len() as u64),
                chunks(&data),
            )
            .await
            .unwrap();
        assert_eq!(count(serial, OperationCode::SendPartialObject), 0);
        assert_eq!(std::fs::read(dir.path().join(&name)).unwrap(), data);
    }
}

#[tokio::test]
async fn an_empty_file_needs_no_windows() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-empty";
    let (_device, storage) = open(dir.path(), serial).await;
    storage
        .upload(None, NewObjectInfo::file("empty.bin", 0), chunks(&[]))
        .await
        .unwrap();
    assert_eq!(count(serial, OperationCode::BeginEditObject), 0);
    assert_eq!(
        std::fs::read(dir.path().join("empty.bin")).unwrap().len(),
        0
    );
}

/// The point of the whole thing: between windows the device is free, so another call runs at
/// once instead of waiting for the rest of the file.
#[tokio::test]
async fn the_device_is_free_between_windows() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("other.txt"), "x").unwrap();
    let serial = "paced-up-between";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(4 * MIB);

    let mut upload = Box::pin(storage.upload_with_pacing(
        None,
        NewObjectInfo::file("between.bin", data.len() as u64),
        chunks(&data),
        UploadPacing::FixedWindow(MIB as u32),
        continue_,
    ));
    // The virtual device answers at once, so the upload runs the first window and parks on the
    // yield before the second.
    poll_until(upload.as_mut(), || {
        count(serial, OperationCode::SendPartialObject) == 1
    })
    .await;
    let listed = storage.list_objects(None).await.unwrap();
    assert!(listed.iter().any(|o| o.filename == "other.txt"));
    assert_eq!(count(serial, OperationCode::SendPartialObject), 1);

    upload.await.unwrap();
    assert_eq!(std::fs::read(dir.path().join("between.bin")).unwrap(), data);
    assert_clean(&storage, serial).await;
}

/// A call from another task waits for the window in flight, not for the windows after it, on a
/// multi-thread runtime too. A bare yield between windows didn't do that there: the woken caller
/// sat behind the uploader in the worker's queue, and the uploader took the lock straight back.
/// On a Pixel a listing waited out two or three 2 s windows (3.1–3.4 s, 2026-11-11).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_from_another_task_waits_for_one_window_on_a_multi_thread_runtime() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("other.txt"), "x").unwrap();
    let serial = "paced-up-fair-mt";
    let (device, storage) = open(dir.path(), serial).await;
    let data = pattern(16 * MIB);
    let window = 64 * 1024;

    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let lister = {
        let storage = device.storages().await.unwrap().remove(0);
        let done = std::sync::Arc::clone(&done);
        tokio::spawn(async move {
            let mut worst = 0;
            let mut listings = 0;
            while listings < 20 {
                // Wait until a window is in flight, then queue a listing behind it.
                let start = count(serial, OperationCode::SendPartialObject);
                while count(serial, OperationCode::SendPartialObject) == start {
                    if done.load(std::sync::atomic::Ordering::Relaxed) {
                        return (worst, listings);
                    }
                    tokio::task::yield_now().await;
                }
                listings += 1;
                let before = count(serial, OperationCode::SendPartialObject);
                storage.list_objects(None).await.unwrap();
                worst = worst.max(count(serial, OperationCode::SendPartialObject) - before);
            }
            (worst, listings)
        })
    };
    storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("fair.bin", data.len() as u64),
            chunks(&data),
            UploadPacing::FixedWindow(window as u32),
            continue_,
        )
        .await
        .unwrap();
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let (worst, listings) = lister.await.unwrap();
    assert!(
        listings >= 10,
        "only {listings} listings ran during the upload (worst wait: {worst} windows)"
    );
    // A window here takes microseconds, so one or two can start between the lister seeing a
    // window in flight and its listing queueing for the lock. Without the handoff it was 80–193.
    assert!(
        worst <= 3,
        "a listing waited for {worst} windows to start after the one in flight"
    );
    assert_eq!(std::fs::read(dir.path().join("fair.bin")).unwrap(), data);
    assert_clean(&storage, serial).await;
}

/// What a stopped windowed upload leaves: exactly the acknowledged windows, the edit ended, the
/// handle surfaced, and a healthy device.
async fn assert_clean_prefix(
    storage: &Storage,
    serial: &str,
    dir: &Path,
    name: &str,
    partial: Option<ObjectHandle>,
    data: &[u8],
    acknowledged: usize,
) {
    let handle = partial.expect("a windowed upload always names the object it stopped");
    let on_disk = std::fs::read(dir.join(name)).unwrap();
    assert_eq!(
        on_disk.len(),
        acknowledged,
        "the stopped window's bytes must be cut away"
    );
    assert_eq!(on_disk, &data[..acknowledged]);
    let info = storage.get_object_info(handle).await.unwrap();
    assert_eq!(info.size, acknowledged as u64);
    assert_clean(storage, serial).await;
    storage
        .delete(handle)
        .await
        .expect("the partial is deletable");
}

#[tokio::test]
async fn a_cancel_mid_window_keeps_exactly_the_acknowledged_windows() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-cancel";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(3 * MIB);

    let err = storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("cancel.bin", data.len() as u64),
            chunks(&data),
            UploadPacing::FixedWindow(MIB as u32),
            |p| {
                if p.bytes_transferred > 3 * MIB as u64 / 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err.source, Error::Cancelled), "{:?}", err.source);
    assert_eq!(count(serial, OperationCode::TruncateObject), 1);
    assert_clean_prefix(
        &storage,
        serial,
        dir.path(),
        "cancel.bin",
        err.partial,
        &data,
        MIB,
    )
    .await;
}

#[tokio::test]
async fn a_cancel_in_the_first_window_keeps_an_empty_object() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-cancel-first";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(2 * MIB);

    let err = storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("first.bin", data.len() as u64),
            chunks(&data),
            UploadPacing::FixedWindow(MIB as u32),
            |_| ControlFlow::Break(()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err.source, Error::Cancelled));
    assert_clean_prefix(
        &storage,
        serial,
        dir.path(),
        "first.bin",
        err.partial,
        &data,
        0,
    )
    .await;
}

#[tokio::test]
async fn a_source_error_mid_window_keeps_exactly_the_acknowledged_windows() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-ioerr";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(3 * MIB);
    let failing = chunks(&data[..MIB + 256 * 1024])
        .chain(futures::stream::iter([Err(std::io::Error::other("disk"))]));

    let err = storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("ioerr.bin", data.len() as u64),
            failing,
            UploadPacing::FixedWindow(MIB as u32),
            continue_,
        )
        .await
        .unwrap_err();

    assert!(matches!(err.source, Error::Io { .. }), "{:?}", err.source);
    assert_clean_prefix(
        &storage,
        serial,
        dir.path(),
        "ioerr.bin",
        err.partial,
        &data,
        MIB,
    )
    .await;
}

#[tokio::test]
async fn a_source_shorter_or_longer_than_declared_stops_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-size";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(3 * MIB);

    let short = storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("short.bin", data.len() as u64),
            chunks(&data[..2 * MIB + 5]),
            UploadPacing::FixedWindow(MIB as u32),
            continue_,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(short.source, Error::Io { .. }),
        "{:?}",
        short.source
    );
    assert_clean_prefix(
        &storage,
        serial,
        dir.path(),
        "short.bin",
        short.partial,
        &data,
        2 * MIB,
    )
    .await;

    let long = storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("long.bin", 2 * MIB as u64),
            chunks(&data),
            UploadPacing::FixedWindow(MIB as u32),
            continue_,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(long.source, Error::InvalidData { .. }),
        "{:?}",
        long.source
    );
    assert_clean_prefix(
        &storage,
        serial,
        dir.path(),
        "long.bin",
        long.partial,
        &data,
        2 * MIB,
    )
    .await;
}

/// A dropped upload leaves nothing: nobody holds its handle, and a file cut short under its real
/// name reads as a complete one. `Drop` can't send anything, so the next operation ends the edit
/// and deletes the object.
async fn assert_dropped_and_deleted(storage: &Storage, serial: &str, dir: &Path, name: &str) {
    assert_eq!(
        virtual_open_edits(serial),
        Some(1),
        "the edit is still open"
    );
    assert!(dir.join(name).exists(), "nothing ran yet");
    assert_clean(storage, serial).await;
    assert!(!dir.join(name).exists(), "the next operation deletes it");
    let listed = storage.list_objects(None).await.unwrap();
    assert!(listed.iter().all(|o| o.filename != name), "{listed:?}");
    assert_eq!(count(serial, OperationCode::DeleteObject), 1);
}

#[tokio::test]
async fn a_dropped_upload_between_windows_is_deleted_by_the_next_operation() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-drop-between";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(4 * MIB);

    let mut upload = Box::pin(storage.upload_with_pacing(
        None,
        NewObjectInfo::file("dropped.bin", data.len() as u64),
        chunks(&data),
        UploadPacing::FixedWindow(MIB as u32),
        continue_,
    ));
    // Parked on the yield before window 2.
    poll_until(upload.as_mut(), || {
        count(serial, OperationCode::SendPartialObject) == 1
    })
    .await;
    drop(upload);
    assert_dropped_and_deleted(&storage, serial, dir.path(), "dropped.bin").await;
}

#[tokio::test]
async fn a_dropped_upload_mid_window_is_deleted_by_the_next_operation() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-drop-mid";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(4 * MIB);
    // One whole window, half of the next, then a source that never answers.
    let source = chunks(&data[..MIB + MIB / 2]).chain(futures::stream::pending());

    let mut upload = Box::pin(storage.upload_with_pacing(
        None,
        NewObjectInfo::file("midway.bin", data.len() as u64),
        source,
        UploadPacing::FixedWindow(MIB as u32),
        continue_,
    ));
    // Run it until window 2 is in flight and waiting for the source. (A command with a data phase
    // is counted once its data arrives, so the count stays at 1.)
    poll_until(upload.as_mut(), || {
        count(serial, OperationCode::SendPartialObject) == 1
    })
    .await;
    for _ in 0..20 {
        assert!(futures::poll!(upload.as_mut()).is_pending());
    }
    assert_eq!(count(serial, OperationCode::SendPartialObject), 1);
    drop(upload);
    assert_dropped_and_deleted(&storage, serial, dir.path(), "midway.bin").await;
}

#[tokio::test]
async fn a_file_bigger_than_the_free_space_is_refused_before_anything_exists() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-full";
    let mut cfg = config(dir.path(), serial);
    cfg.storages[0].capacity = 2 * MIB as u64;
    let device = MtpDevice::builder().open_virtual(cfg).await.unwrap();
    let storage = device.storages().await.unwrap().remove(0);
    let data = pattern(3 * MIB);

    let err = storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("big.bin", data.len() as u64),
            chunks(&data),
            UploadPacing::FixedWindow(MIB as u32),
            continue_,
        )
        .await
        .unwrap_err();
    assert!(matches!(err.source, Error::StorageFull), "{:?}", err.source);
    assert!(err.partial.is_none());
    assert!(!dir.path().join("big.bin").exists());
    assert_eq!(count(serial, OperationCode::SendObjectInfo), 0);
}

/// Android stamps the sent date at `SendObject`, and the windows after it are writes, so the file
/// ends with the time of its last window. Documented, not fixed: Android can't set it afterwards.
#[tokio::test]
async fn a_windowed_upload_gets_the_time_of_its_last_write() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "paced-up-date";
    let (_device, storage) = open(dir.path(), serial).await;
    let sent = crate::mtp::DateTime::new(2020, 1, 2, 3, 4, 5).unwrap();
    let data = pattern(MIB);

    let windowed = storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("dated.bin", data.len() as u64).with_modified(sent),
            chunks(&data),
            UploadPacing::FixedWindow(MIB as u32),
            continue_,
        )
        .await
        .unwrap();
    let modified = storage.get_object_info(windowed).await.unwrap().modified;
    assert!(modified.unwrap().year >= 2026, "{modified:?}");

    // Held open, the default, keeps it (as a Pixel does).
    let held = storage
        .upload(
            None,
            NewObjectInfo::file("kept.bin", data.len() as u64).with_modified(sent),
            chunks(&data),
        )
        .await
        .unwrap();
    let modified = storage.get_object_info(held).await.unwrap().modified;
    assert_eq!(modified.map(|d| d.year), Some(2020), "{modified:?}");
}

// --- The step-by-step upload (`FileUpload`) ---

#[tokio::test]
async fn a_step_by_step_upload_frees_the_device_between_windows() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("other.txt"), "x").unwrap();
    let serial = "file-up-steps";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(3 * MIB);

    let mut upload = storage
        .start_upload(
            None,
            NewObjectInfo::file("steps.bin", data.len() as u64),
            UploadPacing::FixedWindow(MIB as u32),
        )
        .await
        .unwrap();
    assert!(upload.is_windowed());
    let handle = upload
        .handle()
        .expect("a windowed upload has its object at the start");
    assert!(!upload.holds_session());

    let mut held = Vec::new();
    let mut listings = 0;
    for piece in data.chunks(MIB / 2) {
        upload
            .write_chunk(Bytes::copy_from_slice(piece))
            .await
            .unwrap();
        held.push(upload.holds_session());
        if !upload.holds_session() {
            // A whole window just closed: the device answers other calls right away.
            storage.list_objects(None).await.unwrap();
            listings += 1;
        }
    }
    // Half a window in flight, then closed, three times.
    assert_eq!(held, [true, false, true, false, true, false]);
    assert_eq!(listings, 3);
    assert_eq!(upload.bytes_written(), data.len() as u64);
    assert_eq!(upload.finish().await.unwrap(), handle);
    assert_eq!(std::fs::read(dir.path().join("steps.bin")).unwrap(), data);
    assert_clean(&storage, serial).await;
}

#[tokio::test]
async fn a_held_open_step_by_step_upload_holds_the_device_to_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "file-up-held";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(2 * MIB);

    let mut upload = storage
        .start_upload(
            None,
            NewObjectInfo::file("held.bin", data.len() as u64),
            UploadPacing::HeldOpen,
        )
        .await
        .unwrap();
    assert!(!upload.is_windowed());
    assert_eq!(upload.handle(), None);
    assert!(
        !upload.holds_session(),
        "nothing starts before the first chunk"
    );
    for piece in data.chunks(MIB / 2) {
        upload
            .write_chunk(Bytes::copy_from_slice(piece))
            .await
            .unwrap();
        assert!(upload.holds_session());
    }
    let handle = upload.finish().await.unwrap();
    assert_eq!(storage.download_to_vec(handle).await.unwrap(), data);
    assert_eq!(count(serial, OperationCode::SendPartialObject), 0);
}

#[tokio::test]
async fn cancelling_a_step_by_step_upload_keeps_the_acknowledged_windows() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "file-up-cancel";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(3 * MIB);

    let mut upload = storage
        .start_upload(
            None,
            NewObjectInfo::file("cancel.bin", data.len() as u64),
            UploadPacing::FixedWindow(MIB as u32),
        )
        .await
        .unwrap();
    for piece in data[..MIB + MIB / 2].chunks(MIB / 2) {
        upload
            .write_chunk(Bytes::copy_from_slice(piece))
            .await
            .unwrap();
    }
    assert!(upload.holds_session(), "mid-window");
    let partial = upload.cancel().await;
    assert_clean_prefix(
        &storage,
        serial,
        dir.path(),
        "cancel.bin",
        partial,
        &data,
        MIB,
    )
    .await;
}

#[tokio::test]
async fn cancelling_a_held_open_step_by_step_upload_leaves_nothing_on_android() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "file-up-cancel-held";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(2 * MIB);

    let mut upload = storage
        .start_upload(
            None,
            NewObjectInfo::file("gone.bin", data.len() as u64),
            UploadPacing::HeldOpen,
        )
        .await
        .unwrap();
    upload
        .write_chunk(Bytes::copy_from_slice(&data[..MIB]))
        .await
        .unwrap();
    assert_eq!(upload.cancel().await, None);
    assert!(!dir.path().join("gone.bin").exists());
    storage
        .list_objects(None)
        .await
        .expect("the device answers");
}

#[tokio::test]
async fn dropping_a_step_by_step_upload_deletes_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern(3 * MIB);
    // Mid-window, and between windows.
    for (serial, written) in [
        ("file-up-drop-mid", MIB + MIB / 2),
        ("file-up-drop-between", MIB),
    ] {
        let (_device, storage) = open(dir.path(), serial).await;
        let name = format!("{serial}.bin");
        let mut upload = storage
            .start_upload(
                None,
                NewObjectInfo::file(&name, data.len() as u64),
                UploadPacing::FixedWindow(MIB as u32),
            )
            .await
            .unwrap();
        for piece in data[..written].chunks(MIB / 2) {
            upload
                .write_chunk(Bytes::copy_from_slice(piece))
                .await
                .unwrap();
        }
        drop(upload);
        assert_dropped_and_deleted(&storage, serial, dir.path(), &name).await;
    }
}

#[tokio::test]
async fn a_step_by_step_upload_refuses_more_or_less_than_declared() {
    let dir = tempfile::tempdir().unwrap();
    let serial = "file-up-size";
    let (_device, storage) = open(dir.path(), serial).await;
    let data = pattern(3 * MIB);

    let mut long = storage
        .start_upload(
            None,
            NewObjectInfo::file("long.bin", 2 * MIB as u64),
            UploadPacing::FixedWindow(MIB as u32),
        )
        .await
        .unwrap();
    long.write_chunk(Bytes::copy_from_slice(&data[..MIB]))
        .await
        .unwrap();
    let err = long
        .write_chunk(Bytes::copy_from_slice(&data[MIB..3 * MIB]))
        .await
        .unwrap_err();
    assert!(
        matches!(err.source, Error::InvalidData { .. }),
        "{:?}",
        err.source
    );
    assert!(
        long.write_chunk(Bytes::from_static(b"x")).await.is_err(),
        "over"
    );
    assert_clean_prefix(
        &storage,
        serial,
        dir.path(),
        "long.bin",
        err.partial,
        &data,
        MIB,
    )
    .await;

    let mut short = storage
        .start_upload(
            None,
            NewObjectInfo::file("short.bin", data.len() as u64),
            UploadPacing::FixedWindow(MIB as u32),
        )
        .await
        .unwrap();
    short
        .write_chunk(Bytes::copy_from_slice(&data[..MIB + 5]))
        .await
        .unwrap();
    let err = short.finish().await.unwrap_err();
    assert!(matches!(err.source, Error::Io { .. }), "{:?}", err.source);
    assert_clean_prefix(
        &storage,
        serial,
        dir.path(),
        "short.bin",
        err.partial,
        &data,
        MIB,
    )
    .await;
}

mod model {
    //! The virtual device's AOSP model itself, through raw `ptp::` calls.

    use super::*;
    use crate::ptp::{ObjectFormatCode, ObjectHandle as PtpHandle, ObjectInfo, PtpSession};
    use crate::transport::virtual_device::VirtualTransport;
    use crate::transport::Transport;
    use std::sync::Arc;

    async fn session(dir: &Path, serial: &str) -> (PtpSession, crate::ptp::StorageId) {
        let transport: Arc<dyn Transport> = Arc::new(VirtualTransport::new(config(dir, serial)));
        let session = PtpSession::open(transport, 1).await.unwrap();
        session.get_device_info().await.unwrap();
        let storage = session.get_storage_ids().await.unwrap()[0];
        (session, storage)
    }

    fn info(name: &str, size: u64) -> ObjectInfo {
        ObjectInfo {
            format: ObjectFormatCode::Undefined,
            size,
            filename: name.into(),
            ..Default::default()
        }
    }

    async fn one(
        b: &'static [u8],
    ) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Unpin + Send {
        futures::stream::iter([Ok(Bytes::from_static(b))])
    }

    /// An empty object opened for editing, the working shape.
    async fn begin(session: &PtpSession, storage: crate::ptp::StorageId) -> PtpHandle {
        let h = session
            .begin_edit_upload(storage, PtpHandle::SEND_ROOT, &info("m.bin", 0))
            .await
            .unwrap();
        h
    }

    #[tokio::test]
    async fn a_window_on_an_object_not_open_for_editing_wedges_the_device() {
        let dir = tempfile::tempdir().unwrap();
        let serial = "model-noedit";
        let (session, storage) = session(dir.path(), serial).await;
        let h = begin(&session, storage).await;
        session.end_edit_object(h).await.unwrap();

        let r = session
            .send_partial_object(h, 0, 5, one(b"hello").await)
            .await;
        assert!(r.is_err());
        assert_eq!(virtual_device_wedged(serial), Some(true));
        assert!(matches!(
            session.get_storage_ids().await,
            Err(crate::PtpError::Timeout)
        ));
    }

    #[tokio::test]
    async fn a_window_past_the_edits_end_wedges_the_device() {
        let dir = tempfile::tempdir().unwrap();
        let serial = "model-past-end";
        let (session, storage) = session(dir.path(), serial).await;
        let h = begin(&session, storage).await;
        session
            .send_partial_object(h, 0, 5, one(b"hello").await)
            .await
            .unwrap();
        assert_eq!(virtual_device_wedged(serial), Some(false));

        let r = session
            .send_partial_object(h, 6, 5, one(b"world").await)
            .await;
        assert!(r.is_err());
        assert_eq!(virtual_device_wedged(serial), Some(true));
    }

    #[tokio::test]
    async fn a_window_at_the_edits_end_appends_and_a_rewrite_inside_works() {
        let dir = tempfile::tempdir().unwrap();
        let serial = "model-append";
        let (session, storage) = session(dir.path(), serial).await;
        let h = begin(&session, storage).await;
        session
            .send_partial_object(h, 0, 5, one(b"hello").await)
            .await
            .unwrap();
        session
            .send_partial_object(h, 5, 5, one(b"world").await)
            .await
            .unwrap();
        session
            .send_partial_object(h, 0, 1, one(b"J").await)
            .await
            .unwrap();
        assert!(
            session.begin_edit_object(h).await.is_err(),
            "one edit per object"
        );
        session.truncate_object(h, 8).await.unwrap();
        assert_eq!(session.get_object_info(h).await.unwrap().size, 8);
        session.end_edit_object(h).await.unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("m.bin")).unwrap(),
            b"Jellowor"
        );
        assert!(
            session.end_edit_object(h).await.is_err(),
            "the edit is closed"
        );
        assert_eq!(virtual_device_wedged(serial), Some(false));
    }

    /// AOSP creates the file at `SendObject`, so an edit without one finds nothing.
    #[tokio::test]
    async fn skipping_send_object_leaves_a_ghost() {
        let dir = tempfile::tempdir().unwrap();
        let serial = "model-ghost";
        let (session, storage) = session(dir.path(), serial).await;
        let (_, _, h) = session
            .send_object_info(storage, PtpHandle::SEND_ROOT, &info("ghost.bin", 0))
            .await
            .unwrap();

        assert!(session.begin_edit_object(h).await.is_err());
        assert!(session.get_object(h).await.is_err());
        match session.delete_object(h).await {
            Err(crate::PtpError::Protocol { code, .. }) => assert_eq!(u16::from(code), 0x2012),
            other => panic!("{other:?}"),
        }
        assert!(session.get_object_info(h).await.is_ok(), "still listed");
        assert_eq!(virtual_device_wedged(serial), Some(false));
    }

    /// The edit is the server's, not the session's: a new session can end it.
    #[tokio::test]
    async fn an_edit_outlives_the_session_that_opened_it() {
        let dir = tempfile::tempdir().unwrap();
        let serial = "model-outlives";
        let transport: Arc<dyn Transport> =
            Arc::new(VirtualTransport::new(config(dir.path(), serial)));
        let first = PtpSession::open(Arc::clone(&transport), 1).await.unwrap();
        let storage = first.get_storage_ids().await.unwrap()[0];
        let h = first
            .begin_edit_upload(storage, PtpHandle::SEND_ROOT, &info("kept.bin", 0))
            .await
            .unwrap();
        first.close().await.unwrap();

        let second = PtpSession::open(transport, 1).await.unwrap();
        assert!(second.begin_edit_object(h).await.is_err(), "still open");
        second.end_edit_object(h).await.unwrap();
        assert_eq!(virtual_open_edits(serial), Some(0));
    }

    #[tokio::test]
    async fn setting_the_modification_date_is_refused_like_aosp() {
        let dir = tempfile::tempdir().unwrap();
        let (session, storage) = session(dir.path(), "model-setdate").await;
        let h = begin(&session, storage).await;
        session.end_edit_object(h).await.unwrap();
        let date = crate::ptp::PropertyValue::String("20200102T030405".into())
            .to_bytes()
            .unwrap();
        let r = session
            .set_object_prop_value(h, crate::ptp::ObjectPropertyCode::DateModified, &date)
            .await;
        match r {
            Err(crate::PtpError::Protocol { code, .. }) => assert_eq!(u16::from(code), 0xA80A),
            other => panic!("{other:?}"),
        }
    }
}
