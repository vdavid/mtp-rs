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

#[tokio::test]
async fn upload_goes_out_in_windows_by_default() {
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

#[tokio::test]
async fn held_open_pacing_and_the_opt_out_send_one_send_object() {
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

    let serial = "paced-up-optout";
    let device = MtpDevice::builder()
        .use_windowed_uploads(false)
        .open_virtual(config(dir.path(), serial))
        .await
        .unwrap();
    let storage = device.storages().await.unwrap().remove(0);
    storage
        .upload_with_pacing(
            None,
            NewObjectInfo::file("optout.bin", data.len() as u64),
            chunks(&data),
            UploadPacing::FixedWindow(MIB as u32),
            continue_,
        )
        .await
        .unwrap();
    assert_eq!(count(serial, OperationCode::SendPartialObject), 0);
    assert_eq!(std::fs::read(dir.path().join("optout.bin")).unwrap(), data);
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
    // The virtual device answers at once, so the first poll runs the first window and parks on
    // the yield before the second.
    assert!(futures::poll!(upload.as_mut()).is_pending());
    assert_eq!(count(serial, OperationCode::SendPartialObject), 1);
    let listed = storage.list_objects(None).await.unwrap();
    assert!(listed.iter().any(|o| o.filename == "other.txt"));
    assert_eq!(count(serial, OperationCode::SendPartialObject), 1);

    upload.await.unwrap();
    assert_eq!(std::fs::read(dir.path().join("between.bin")).unwrap(), data);
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
        .upload_with_progress(
            None,
            NewObjectInfo::file("first.bin", data.len() as u64),
            chunks(&data),
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

/// `Drop` can't send anything, so a dropped upload's edit is ended by the next operation.
#[tokio::test]
async fn a_dropped_upload_between_windows_is_ended_by_the_next_operation() {
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
    assert!(futures::poll!(upload.as_mut()).is_pending());
    drop(upload);
    assert_eq!(virtual_open_edits(serial), Some(1));

    assert_clean(&storage, serial).await;
    assert_eq!(
        std::fs::read(dir.path().join("dropped.bin")).unwrap(),
        &data[..MIB]
    );
}

#[tokio::test]
async fn a_dropped_upload_mid_window_is_ended_by_the_next_operation() {
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
    assert!(futures::poll!(upload.as_mut()).is_pending()); // parked before window 2
                                                           // Inside window 2: its command went out, and the device is reading its data. (A command
                                                           // with a data phase is counted once its data arrives, so the count still says 1.)
    assert!(futures::poll!(upload.as_mut()).is_pending());
    assert_eq!(count(serial, OperationCode::SendPartialObject), 1);
    drop(upload);

    assert_clean(&storage, serial).await;
    assert_eq!(
        std::fs::read(dir.path().join("midway.bin")).unwrap(),
        &data[..MIB]
    );
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
        .upload(
            None,
            NewObjectInfo::file("big.bin", data.len() as u64),
            chunks(&data),
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

    let handle = storage
        .upload(
            None,
            NewObjectInfo::file("dated.bin", data.len() as u64).with_modified(sent),
            chunks(&data),
        )
        .await
        .unwrap();
    let modified = storage
        .get_object_info(handle)
        .await
        .unwrap()
        .modified
        .unwrap();
    assert!(modified.year >= 2026, "{modified:?}");
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
