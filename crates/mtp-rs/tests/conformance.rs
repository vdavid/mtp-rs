//! Backend-conformance suite: behavioral tests written against the **public** backend-neutral
//! `mtp::` API. The same tests are meant to run against every backend; this file runs them against
//! the only backend available without hardware — `UsbBackend` driving the virtual device.
//!
//! The assertion bodies live in [`common`] as reusable `run_*` harness functions, so the WPD
//! suite (`conformance_wpd.rs`) can point the exact same assertions at a real device. Each test
//! here is a thin wrapper that opens the virtual device and calls the corresponding harness fn with
//! `parent = None` (the virtual storage root is writable) and `expects_partial = true` (the
//! two-phase USB backend surfaces a partial handle on a cancelled upload).
//!
//! Run with: `cargo test -p mtp-rs --features virtual-device --test conformance`.

#![cfg(feature = "virtual-device")]

mod common;

use common::open_device;

#[tokio::test]
async fn device_info_and_capabilities_are_neutral() {
    let (device, _dir) = open_device("conf-info").await;

    let info = device.device_info();
    assert_eq!(info.manufacturer, "TestCorp");
    assert_eq!(info.model, "Conformance Phone");
    assert_eq!(info.serial_number, "conf-info");

    // The virtual device advertises the full op set, so these should all be available.
    let caps = device.capabilities();
    assert!(caps.can_upload);
    assert!(caps.can_delete);
    assert!(caps.can_create_folder);
    assert!(caps.can_move);
    assert!(caps.can_copy);
    assert!(caps.supports_partial_download);
    assert!(device.supports_rename());
    assert!(device.supports_upload());
}

#[tokio::test]
async fn storages_carry_neutral_info() {
    let (device, _dir) = open_device("conf-storages").await;
    let storages = device.storages().await.unwrap();
    assert_eq!(storages.len(), 1);
    let info = storages[0].info();
    assert_eq!(info.description, "Internal Storage");
    assert!(info.total_capacity > 0);
    assert!(info.is_writable);
    // The id round-trips: fetching by it yields the same storage.
    assert_eq!(storages[0].id(), info.id);
}

#[tokio::test]
async fn round_trip_upload_list_download_delete() {
    let (device, _dir) = open_device("conf-roundtrip").await;
    let storage = &device.storages().await.unwrap()[0];
    common::run_round_trip(storage, None).await;
}

#[tokio::test]
async fn ranged_and_resumable_download() {
    let (device, _dir) = open_device("conf-ranged").await;
    let storage = &device.storages().await.unwrap()[0];
    common::run_ranged_and_resumable(storage, None).await;
}

#[tokio::test]
async fn windowed_download_reassembles_whole_file() {
    let (device, _dir) = open_device("conf-windowed").await;
    let storage = &device.storages().await.unwrap()[0];
    common::run_windowed_download(storage, None).await;
}

#[tokio::test]
async fn create_folder_rename_move_copy() {
    let (device, _dir) = open_device("conf-fsops").await;
    let storage = &device.storages().await.unwrap()[0];
    common::run_create_folder_rename_move_copy(storage, None).await;
}

#[tokio::test]
async fn name_too_long_is_refused() {
    let (device, _dir) = open_device("conf-name-too-long").await;
    let storage = &device.storages().await.unwrap()[0];
    common::run_name_too_long_is_refused(storage, None).await;
}

#[tokio::test]
async fn recursive_listing_walks_the_tree() {
    let (device, _dir) = open_device("conf-recursive").await;
    let storage = &device.storages().await.unwrap()[0];
    common::run_recursive_listing(storage, None).await;
}

#[tokio::test]
async fn upload_with_progress_reports_and_completes() {
    let (device, _dir) = open_device("conf-progress").await;
    let storage = &device.storages().await.unwrap()[0];
    common::run_upload_with_progress(storage, None).await;
}

#[tokio::test]
async fn upload_cancel_surfaces_partial_handle() {
    let (device, _dir) = open_device("conf-upload-cancel").await;
    let storage = &device.storages().await.unwrap()[0];
    // The two-phase USB backend surfaces a partial handle on a cancelled upload.
    common::run_upload_cancel(storage, None, true).await;
}

#[tokio::test]
async fn download_cancel_leaves_session_usable() {
    let (device, _dir) = open_device("conf-dl-cancel").await;
    let storage = &device.storages().await.unwrap()[0];
    common::run_download_cancel(storage, None).await;
}

#[tokio::test]
async fn download_drop_leaves_session_usable() {
    let (device, _dir) = open_device("conf-dl-drop").await;
    let storage = &device.storages().await.unwrap()[0];
    common::run_download_drop(storage, None).await;
}

#[tokio::test]
async fn list_cancel_token_bails() {
    let (device, _dir) = open_device("conf-list-cancel").await;
    let storage = &device.storages().await.unwrap()[0];
    common::run_list_cancel_token(storage, None).await;
}

#[tokio::test]
async fn thumbnail_unsupported_errors_gracefully() {
    let (device, _dir) = open_device("conf-thumb").await;
    let storage = &device.storages().await.unwrap()[0];
    common::run_thumbnail_unsupported(storage, None).await;
}
