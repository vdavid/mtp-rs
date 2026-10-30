//! Shared, backend-neutral conformance harness.
//!
//! Each `run_*` function below is the *body* of one behavioral conformance assertion, written
//! against the **public** `mtp_rs::` API only (no PTP/USB/WPD internals). They take an already-open
//! [`Storage`] plus a **writable parent** handle so the same assertions can run against any backend:
//!
//! - The virtual device allows uploads to the storage root, so the conformance suite passes
//!   `parent = None`.
//! - Real Android/WPD rejects root uploads, so the WPD suite passes `parent = Some(<scoped folder>)`.
//!
//! Two backend divergences are parameterized rather than hard-coded:
//!
//! - **Writable parent** (`parent: Option<ObjectHandle>`): every fn creates and works under the
//!   supplied parent instead of assuming the root is writable.
//! - **Upload-cancel partial handle** (`expects_partial: bool`): the two-phase USB/virtual backend
//!   leaves `UploadError::partial = Some(handle)` after a cancelled upload; WPD commits
//!   transactionally and leaves `None`.
//!
//! Lives in `tests/common/` so cargo does not treat it as its own test binary; each test file
//! includes it with `mod common;`.

// Not every test binary that includes this module uses every helper (the WPD suite skips
// `open_device`, the virtual suite is the only `expects_partial = true` caller, etc.).
#![allow(dead_code)]

use bytes::Bytes;
use mtp_rs::{ByteRange, Error, NewObjectInfo, ObjectHandle, Storage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Open a writable single-storage virtual device backed by a fresh temp dir.
///
/// Returns the open device plus the `TempDir` guard (kept alive for the test's duration). Only
/// available with the `virtual-device` feature; the WPD suite opens the real device itself.
#[cfg(feature = "virtual-device")]
pub async fn open_device(serial: &str) -> (mtp_rs::MtpDevice, tempfile::TempDir) {
    use mtp_rs::{VirtualDeviceConfig, VirtualStorageConfig};

    let dir = tempfile::tempdir().unwrap();
    let config = VirtualDeviceConfig {
        manufacturer: "TestCorp".into(),
        model: "Conformance Phone".into(),
        serial: serial.into(),
        storages: vec![VirtualStorageConfig {
            description: "Internal Storage".into(),
            capacity: 1024 * 1024 * 1024,
            backing_dir: dir.path().to_path_buf(),
            read_only: false,
        }],
        event_poll_interval: Duration::ZERO,
        watch_backing_dirs: false,
        ..Default::default()
    };
    let device = mtp_rs::MtpDevice::builder()
        .open_virtual(config)
        .await
        .unwrap();
    (device, dir)
}

/// Stream a byte slice as an owned upload stream.
pub fn upload_stream(
    data: &[u8],
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Unpin {
    futures::stream::once(futures::future::ok(Bytes::copy_from_slice(data)))
}

/// Upload `data` as `name` into `parent`, returning the new handle.
pub async fn upload(
    storage: &Storage,
    parent: Option<ObjectHandle>,
    name: &str,
    data: &[u8],
) -> ObjectHandle {
    let info = NewObjectInfo::file(name, data.len() as u64);
    storage
        .upload(parent, info, upload_stream(data))
        .await
        .expect("upload should succeed")
}

/// Round trip: upload → list → object_info → streaming + buffered download → delete.
pub async fn run_round_trip(storage: &Storage, parent: Option<ObjectHandle>) {
    let content = b"the quick brown fox jumps over the lazy dog".repeat(50);
    let handle = upload(storage, parent, "round.bin", &content).await;

    // It shows up in a listing of the parent.
    let listed = storage.list_objects(parent).await.unwrap();
    let found = listed
        .iter()
        .find(|o| o.filename == "round.bin")
        .expect("uploaded file should appear in the listing");
    assert!(found.is_file());
    assert_eq!(found.size, content.len() as u64);

    // object_info resolves the same metadata.
    let info = storage.get_object_info(handle).await.unwrap();
    assert_eq!(info.filename, "round.bin");
    assert_eq!(info.size, content.len() as u64);

    // Streaming download of the whole file matches byte-for-byte.
    let dl = storage.download(handle, ByteRange::Full).await.unwrap();
    assert_eq!(dl.size(), content.len() as u64);
    let got = dl.collect().await.unwrap();
    assert_eq!(got, content);

    // Buffered convenience matches too.
    let buffered = storage.download_to_vec(handle).await.unwrap();
    assert_eq!(buffered, content);

    // Delete removes it.
    storage.delete(handle).await.unwrap();
    assert!(storage.get_object_info(handle).await.is_err());
}

/// Ranged (`read_range`) and resumable (`ByteRange::From`) downloads, plus offset edge cases.
pub async fn run_ranged_and_resumable(storage: &Storage, parent: Option<ObjectHandle>) {
    let content: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
    let handle = upload(storage, parent, "ranged.bin", &content).await;

    // A bounded range via the buffered primitive.
    let mid = storage.read_range(handle, 1000, 500).await.unwrap();
    assert_eq!(mid, &content[1000..1500]);

    // Resume from an offset via the streaming primitive: size() stays the FULL size.
    let offset = 1024u64;
    let tail = storage
        .download(handle, ByteRange::From(offset))
        .await
        .unwrap();
    assert_eq!(
        tail.size(),
        content.len() as u64,
        "size() reports the whole file even for a resumed download"
    );
    let tail_bytes = tail.collect().await.unwrap();
    assert_eq!(tail_bytes, &content[offset as usize..]);

    // offset == size yields a clean, empty stream.
    let empty = storage
        .download(handle, ByteRange::From(content.len() as u64))
        .await
        .unwrap();
    assert!(empty.collect().await.unwrap().is_empty());

    // offset > size is a precondition error, before any I/O.
    let err = storage
        .download(handle, ByteRange::From(content.len() as u64 + 1))
        .await;
    assert!(matches!(err, Err(Error::InvalidData { .. })));

    storage.delete(handle).await.unwrap();
}

/// Windowed download reassembles the whole file, and a windowed resume covers the tail.
pub async fn run_windowed_download(storage: &Storage, parent: Option<ObjectHandle>) {
    let content: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
    let handle = upload(storage, parent, "windowed.bin", &content).await;

    let mut dl = storage
        .download_windowed(handle, ByteRange::Full, 512)
        .await
        .unwrap();
    assert_eq!(dl.size(), content.len() as u64);

    let mut assembled = Vec::new();
    while let Some(window) = dl.next_window().await {
        assembled.extend_from_slice(&window.unwrap());
    }
    assert_eq!(assembled, content);

    // A windowed resume from an offset covers exactly the tail.
    let offset = 2048u64;
    let mut resume = storage
        .download_windowed(handle, ByteRange::From(offset), 256)
        .await
        .unwrap();
    let mut tail = Vec::new();
    while let Some(window) = resume.next_window().await {
        tail.extend_from_slice(&window.unwrap());
    }
    assert_eq!(tail, &content[offset as usize..]);

    storage.delete(handle).await.unwrap();
}

/// create_folder + rename + copy + move, verifying handles and parents stay consistent.
pub async fn run_create_folder_rename_move_copy(storage: &Storage, parent: Option<ObjectHandle>) {
    // Three folders under the parent. `copy_dst` and `move_dst` are kept separate so the copy and
    // the move don't collide on the same filename (a same-name move into a folder overwrites, which
    // is device behavior, not what this test is checking).
    let src = storage.create_folder(parent, "src").await.unwrap();
    let copy_dst = storage.create_folder(parent, "copy_dst").await.unwrap();
    let move_dst = storage.create_folder(parent, "move_dst").await.unwrap();

    // A file inside src.
    let file = upload(storage, Some(src), "a.txt", b"hello world").await;

    // Rename it in place.
    storage.rename(file, "renamed.txt").await.unwrap();
    let info = storage.get_object_info(file).await.unwrap();
    assert_eq!(info.filename, "renamed.txt");

    // Copy it into copy_dst (a NEW handle, original stays under src).
    let copy = storage.copy_object(file, copy_dst, None).await.unwrap();
    assert_ne!(copy, file);
    let copy_info = storage.get_object_info(copy).await.unwrap();
    assert_eq!(copy_info.filename, "renamed.txt");
    assert_eq!(copy_info.parent, copy_dst);
    assert_eq!(storage.get_object_info(file).await.unwrap().parent, src);

    // Move the original into move_dst.
    storage.move_object(file, move_dst, None).await.unwrap();
    assert_eq!(
        storage.get_object_info(file).await.unwrap().parent,
        move_dst
    );

    // The copy is untouched under copy_dst; the moved original is the lone child of move_dst.
    let in_copy = storage.list_objects(Some(copy_dst)).await.unwrap();
    assert_eq!(in_copy.len(), 1, "copy_dst holds the copy");
    let in_move = storage.list_objects(Some(move_dst)).await.unwrap();
    assert_eq!(in_move.len(), 1, "move_dst holds the moved original");
    // src is now empty (the original moved out).
    assert!(storage.list_objects(Some(src)).await.unwrap().is_empty());

    // Clean up the scratch folders.
    storage.delete(src).await.unwrap();
    storage.delete(copy_dst).await.unwrap();
    storage.delete(move_dst).await.unwrap();
}

/// A name MTP can't carry (255 UTF-16 units) is refused with `NameTooLong` by upload,
/// create_folder, and rename alike, nothing changes on the device, and 254 units still works.
pub async fn run_name_too_long_is_refused(storage: &Storage, parent: Option<ObjectHandle>) {
    let too_long = "a".repeat(mtp_rs::MAX_NAME_UNITS + 1);
    let longest = "b".repeat(mtp_rs::MAX_NAME_UNITS);

    let err = storage
        .upload(
            parent,
            NewObjectInfo::file(too_long.clone(), 2),
            upload_stream(b"hi"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err.source, Error::NameTooLong { units: 255 }),
        "upload: {err:?}"
    );
    assert!(err.partial.is_none(), "refused before any object exists");

    let err = storage.create_folder(parent, &too_long).await.unwrap_err();
    assert!(
        matches!(err, Error::NameTooLong { units: 255 }),
        "create_folder: {err:?}"
    );

    // An emoji is two units, so 128 of them is 256 units in only 128 chars.
    let file = upload(storage, parent, "short.txt", b"hi").await;
    let err = storage
        .rename(file, &"\u{1F600}".repeat(128))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::NameTooLong { units: 256 }),
        "rename: {err:?}"
    );
    assert_eq!(
        storage.get_object_info(file).await.unwrap().filename,
        "short.txt"
    );

    // Nothing reached the device, so the session is fine and the longest legal name goes through.
    let listed = storage.list_objects(parent).await.unwrap();
    assert!(listed.iter().all(|o| o.filename != too_long));
    storage.rename(file, &longest).await.unwrap();
    assert_eq!(
        storage.get_object_info(file).await.unwrap().filename,
        longest
    );
    storage.delete(file).await.unwrap();
}

/// Recursive listing walks the whole subtree.
pub async fn run_recursive_listing(storage: &Storage, parent: Option<ObjectHandle>) {
    let top = storage.create_folder(parent, "top").await.unwrap();
    let sub = storage.create_folder(Some(top), "sub").await.unwrap();
    upload(storage, Some(top), "top.txt", b"top").await;
    upload(storage, Some(sub), "deep.txt", b"deep").await;

    let all = storage.list_objects_recursive(Some(top)).await.unwrap();
    let names: Vec<&str> = all.iter().map(|o| o.filename.as_str()).collect();
    assert!(names.contains(&"sub"));
    assert!(names.contains(&"top.txt"));
    assert!(names.contains(&"deep.txt"));

    storage.delete(top).await.unwrap();
}

/// Upload with a progress callback reports at least once and produces a complete file.
pub async fn run_upload_with_progress(storage: &Storage, parent: Option<ObjectHandle>) {
    let content = vec![7u8; 4096];
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = Arc::clone(&calls);
    let info = NewObjectInfo::file("progress.bin", content.len() as u64);

    let handle = storage
        .upload_with_progress(parent, info, upload_stream(&content), move |p| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            assert!(p.total_bytes == Some(content.len() as u64));
            std::ops::ControlFlow::Continue(())
        })
        .await
        .expect("upload should complete");

    assert!(calls.load(Ordering::SeqCst) >= 1, "progress should fire");
    assert_eq!(storage.download_to_vec(handle).await.unwrap().len(), 4096);

    storage.delete(handle).await.unwrap();
}

/// A cancelled upload fails with [`Error::Cancelled`].
///
/// `expects_partial` parameterizes the backend divergence: the two-phase USB/virtual backend leaves
/// a partial object (`UploadError::partial = Some(handle)`) that the caller must clean up; WPD
/// commits transactionally and leaves `None`.
pub async fn run_upload_cancel(
    storage: &Storage,
    parent: Option<ObjectHandle>,
    expects_partial: bool,
) {
    let info = NewObjectInfo::file("cancelled.bin", 5);
    let err = storage
        .upload_with_progress(parent, info, upload_stream(b"hello"), |_| {
            std::ops::ControlFlow::Break(())
        })
        .await
        .expect_err("a cancelled upload should fail");

    assert!(matches!(err.source, Error::Cancelled));
    if expects_partial {
        assert!(
            err.partial.is_some(),
            "two-phase backends must surface the partial handle so the caller can clean up"
        );
        // And the caller can delete it with the surfaced handle.
        storage.delete(err.partial.unwrap()).await.unwrap();
    } else {
        assert!(
            err.partial.is_none(),
            "transactional backends commit atomically: no partial handle to clean up"
        );
    }
}

/// A cancelled streaming download drains the pipe and leaves the session usable.
pub async fn run_download_cancel(storage: &Storage, parent: Option<ObjectHandle>) {
    let content = vec![3u8; 64 * 1024];
    let handle = upload(storage, parent, "big.bin", &content).await;

    // Start a streaming download, pull one chunk, then cancel and drop. The download holds the
    // session for the whole transfer, so it must be dropped (here, end of scope) after cancel()
    // before the session is free again — the documented contract.
    {
        let mut dl = storage.download(handle, ByteRange::Full).await.unwrap();
        let first = dl.next_chunk().await.expect("at least one chunk").unwrap();
        assert!(!first.is_empty());
        dl.cancel(Duration::from_millis(300)).await.unwrap();
    }

    // The session is healthy for the next operation (cancel drained the pipe).
    let listed = storage.list_objects(parent).await.unwrap();
    assert!(listed.iter().any(|o| o.filename == "big.bin"));

    storage.delete(handle).await.unwrap();
}

/// Dropping a streaming download without cancel still self-heals the session (lazy recovery).
pub async fn run_download_drop(storage: &Storage, parent: Option<ObjectHandle>) {
    let content = vec![9u8; 64 * 1024];
    let handle = upload(storage, parent, "drop.bin", &content).await;

    // Start a download, pull one chunk, then DROP it without cancelling. The session self-heals on
    // the next operation (lazy recovery).
    {
        let mut dl = storage.download(handle, ByteRange::Full).await.unwrap();
        let _first = dl.next_chunk().await.expect("at least one chunk").unwrap();
        // dl dropped here
    }

    let listed = storage.list_objects(parent).await.unwrap();
    assert!(listed.iter().any(|o| o.filename == "drop.bin"));

    storage.delete(handle).await.unwrap();
}

/// A pre-cancelled [`CancelToken`] bails the listing before any roundtrip.
pub async fn run_list_cancel_token(storage: &Storage, parent: Option<ObjectHandle>) {
    use mtp_rs::CancelToken;

    let mut handles = Vec::new();
    for i in 0..5 {
        handles.push(upload(storage, parent, &format!("f{i}.txt"), b"x").await);
    }

    let cancel = CancelToken::new();
    cancel.cancel();
    let result = storage
        .list_objects_with_cancel(parent, Some(&cancel))
        .await;
    assert!(matches!(result, Err(Error::Cancelled)));

    for handle in handles {
        storage.delete(handle).await.unwrap();
    }
}

/// Requesting a thumbnail of a non-image surfaces an error rather than panicking or hanging.
pub async fn run_thumbnail_unsupported(storage: &Storage, parent: Option<ObjectHandle>) {
    let handle = upload(storage, parent, "pic.jpg", b"not a real jpeg").await;
    // No real thumbnail is available for this junk payload: it must surface an error.
    let result = storage.thumbnail(handle).await;
    assert!(result.is_err(), "no thumbnail expected for a non-image");

    storage.delete(handle).await.unwrap();
}
