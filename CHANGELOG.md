# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This file covers both published crates in the workspace:

- `mtp-rs` (the library)
- `mtp-rs-cli` (the CLI binary)

Entries are grouped by release. Each entry tags which crate it applies to with **[lib]**, **[cli]**, or **[workspace]** for repo-wide changes.

## [Unreleased]

### Fixed

- **[lib] An upload can no longer be broken by another call on the same device starting at the wrong moment.** An upload is two requests, one describing the file and one sending its bytes, and a call from another task (a folder listing, say) could slip in between. Android then forgets the description and refuses the bytes, which made the upload fail, and on a Pixel 9 Pro XL a refused upload left the phone unreachable until it was replugged. The two requests now go out back to back. [#62](https://github.com/vdavid/mtp-rs/issues/62)
- **[lib] On Windows, uploads now carry the file's modification date.** The date set with `NewObjectInfo::with_modified` never reached the phone, so every file uploaded through the Windows backend arrived dated "now". It now goes out with the upload. A date with a UTC offset is sent as that moment in the PC's time zone, since the phone reads the date as its own local time. Not yet checked on a Windows machine with a phone. [#68](https://github.com/vdavid/mtp-rs/issues/68)

- **[lib] During a big paced download, a call from another task now waits for one window, not several.** Between windows the download only yielded, and on a multi-thread runtime (tokio's default, so most apps) that didn't let a waiting call in: the download took the device straight back, and a folder listing, which on Android is several requests, lost it again between its requests. A listing could wait out the whole file. Now the download waits between windows until the device has been free for a moment (at most 0.5 s), and costs nothing when no other call is waiting. Windowed uploads (below) do the same: on a Pixel 9 Pro XL, listings during a 1 GiB upload in 2 s windows waited 1.3–1.7 s, against 2.7–3.6 s before.

- **[cli] `get` and `put` keep file dates.** `get` gave every copy today's date, and `put` sent no date at all, so a backup or a restore lost every file's modification date. `get` now dates each file (and, in a folder download, each folder) with the device's modification date, and `put` sends the local file's date with the upload. Most devices write dates with no time zone, and `get` reads those as local time on your computer, at the offset in force on that date; set `TZ` to pick another zone. `put` sends the date in UTC, which Android reads as the right moment whatever the phone's zone. The phone decides whether to keep it: Samsung phones date every upload "now" ([#68](https://github.com/vdavid/mtp-rs/issues/68)), and uploads on Windows don't carry the date yet. [#70](https://github.com/vdavid/mtp-rs/issues/70)

### Added

- **[lib] Uploads to Android phones can go out in windows, so the phone stays free for other calls during a big upload.** Opt in per upload with `Storage::upload_with_pacing(.., UploadPacing::MaxWindowTime(DEFAULT_UPLOAD_WINDOW_TIME), ..)`, or for a whole device with `MtpDeviceBuilder::upload_pacing`. A folder listing, a thumbnail, or a download from another task then waits for one window of about two seconds instead of the whole upload. The library writes the file through Android's edit operations, which phones list as 0x95C2–0x95C5 (`Capabilities::supports_windowed_upload`), and frees the device between windows, the way downloads already did. Each window costs 1–3 ms on a Pixel 9 Pro XL and a Galaxy S23 Ultra, under 1% at that size. Other devices and Windows upload in one piece whatever the pacing says. `Storage::upload` stays held open, because windows behave differently on those phones in ways an app may depend on:
  - **The modification date is lost.** Android stamps the date in `NewObjectInfo` when it creates the file, and the writes after that move it to the time of the last window. Android has no way to set it afterwards. A held-open upload keeps it on a Pixel; a Galaxy S23 Ultra drops it either way. Choosing a better default is [#68](https://github.com/vdavid/mtp-rs/issues/68).
  - **A stopped upload keeps what the phone acknowledged.** After a `ControlFlow::Break`, a source error, or a device error, `UploadError::partial` is `Some(handle)`, and the object holds exactly the windows that arrived, a clean prefix of the file. A held-open upload to Android leaves nothing. The library doesn't delete it; delete it yourself if you don't want it.
  - **A dropped upload leaves nothing.** Nobody gets the handle of a windowed upload whose future was dropped, and a file cut short under its real name reads as a complete one, so the next call on the device deletes it.

  [#62](https://github.com/vdavid/mtp-rs/issues/62)
- **[lib] `Storage::start_upload` and `FileUpload`: an upload fed chunk by chunk**, the counterpart of `FileDownload`. `write_chunk`, `finish`, `cancel`, and `holds_session()`, which is `false` between windows, so an app with its own lock around the device (a file manager) can hold it only while a window is in flight and let a listing in between. `examples/upload_with_app_lock.rs` shows it. [#62](https://github.com/vdavid/mtp-rs/issues/62)
- **[lib] New API**: `UploadPacing` (`HeldOpen`, the default, `MaxWindowTime`, `FixedWindow`), `DEFAULT_UPLOAD_WINDOW_TIME` (2 s), `Storage::upload_with_pacing`, `Storage::start_upload`, `FileUpload`, `MtpDeviceBuilder::upload_pacing`, and `Capabilities::supports_windowed_upload`. [#62](https://github.com/vdavid/mtp-rs/issues/62)
- **[lib] A windowed upload checks the storage's free space first.** It declares the file with size 0 (the phone would otherwise wait for that many bytes before the first window), so the phone can't refuse a file too big for the storage up front. A file bigger than the free space fails with `Error::StorageFull` before anything is created.
- **[cli]** `mtp-rs doctor` reports `windowed_upload`.
- **[lib] The virtual device models Android's edit operations** (`VirtualDeviceConfig::supports_edit_objects`, on by default in Android mode), including the two refusals that wedge a real phone: a window on an object not open for editing, or past the end of what the phone holds, is refused before its data is read, and the data then wedges the virtual device (`virtual_device_wedged`). A `SendObject` refused for want of a `SendObjectInfo` now wedges it too. Also modeled: the ghost object a skipped `SendObject` leaves, an edit that outlives its session (`virtual_open_edits`), the bytes a stopped window leaves past the edit, and AOSP's refusal to set `DateModified`. [#62](https://github.com/vdavid/mtp-rs/issues/62)
- **[cli] `ls` shows dates.** `ls --json` gives every object `modified` and `created`, in ISO 8601 as the device wrote them (`2026-11-03T14:22:05`, with an offset only when the device sent one), or `null` when there's none. `ls --long` (`-l`) adds a modification date column to the plain listing. New keys only, so scripts reading today's output keep working. [#69](https://github.com/vdavid/mtp-rs/issues/69)
- **[lib] The virtual device forgets a pending upload description like Android does.** With `android: true`, any other command between `SendObjectInfo` and `SendObject` makes the `SendObject` fail with `NoValidObjectInfo` (0x2015), so a test can catch a host that lets another call in between. [#62](https://github.com/vdavid/mtp-rs/issues/62)

### Changed

- **[lib] An upload cancelled from its progress callback before any byte went out creates nothing.** A held-open upload now starts with its first chunk, so a `ControlFlow::Break` on that chunk leaves no object behind (`UploadError::partial` is `None`), where a device that keeps partials (a camera) used to keep an empty one.

## [0.38.1] - 2026-11-11

Library `0.38.1` and CLI `0.11.9`. Big downloads on Windows stay fast while an app browses the phone, stopping an upload on Android no longer makes the next call time out, and uploads no longer block a runtime thread. No API changes, so `^0.38` users get it through `cargo update`.

### Fixed

- **[cli]** The CLI ships with the library fixes below in `0.11.9`.

- **[lib] On Windows, a big download no longer slows to a crawl when other calls run between its windows.** Each such call made the next window reopen the file on the phone, and on phones that can't seek, read everything before the window again. The download counted that as transfer time, so it shrank its windows, which let in more calls and caused more reopens: a 1 GB copy fell to under 1 MB/s while the app browsed the phone. Window sizes now come from the time spent receiving data only. [#61](https://github.com/vdavid/mtp-rs/issues/61)
- **[lib] On Windows, calls between the windows of a download now cost the download nothing.** Each window is now its own partial read on the phone, sent through the Windows MTP driver, so there's no file stream to reopen after a call. A 1 GiB copy from a Pixel 9 Pro XL with a folder listing, a photo, and a thumbnail loaded between every few windows took 34 s, against 138 s before; with those calls between every window, 60–70 s, where before it fell to 2 MB/s and the phone stopped answering. Phones and drivers without that partial read keep the old path. [#61](https://github.com/vdavid/mtp-rs/issues/61)
- **[lib] On Windows, windows of a download stay near their target length when the app takes time with each chunk.** The Windows backend reads ahead while the app handles earlier chunks, and that time wasn't counted as the phone's, so windows meant to take 500 ms grew to almost 6 s when the app wrote each chunk to disk, and a cancel waited that long. [#61](https://github.com/vdavid/mtp-rs/issues/61)
- **[lib] Cancelling an upload on Android no longer makes the next call time out.** When an upload stopped partway (a `ControlFlow::Break` from the progress callback, a source read error, or a dropped future), the phone kept reading what came next as file data, so the next operation never got an answer and failed after the 30 s timeout. The library now tells the phone the data has ended, so the upload returns within about half a second and the next call works right away. Android deletes the half-written file at that point, so `UploadError::partial` is `None` there. [#62](https://github.com/vdavid/mtp-rs/issues/62)
- **[lib] The virtual device hands a held-open download over in USB-sized chunks.** A `GetObject` held open on a device without partial reads came back from the virtual device as one chunk holding the whole object, while real USB reads it 64 KiB at a time. A consumer that bounds memory per `next_chunk` call couldn't test that bound against the virtual device. The virtual transport now honors the read size, so `next_chunk` yields at most 64 KiB, as on hardware. [#63](https://github.com/vdavid/mtp-rs/issues/63)
- **[lib] An upload no longer ties up a thread of your async runtime, and a timeout around it can stop it partway.** Each USB transfer of an upload (and every command sent to the device) waited with a blocking call, so the upload held an executor thread from start to end, and a `timeout` or `select` around it could only act between reads of your source. With a source that's always ready, a 300 ms timeout around a 268 MB upload to a Pixel 9 Pro XL fired only after the whole file had gone out (1.6 s). The waits are async now, so the same timeout stops it at 300 ms, and the next call answers in ~0.2 s with nothing left on the phone.

### Added

- **[lib] The `tracing` feature now covers the Windows backend too**, so you can see where a slow Windows call spends its time: how long each request waited and took, when a file read stream is reopened or released and what that cost, and whether the phone could seek or had to read from the start of the file. Filter on `mtp_rs::mtp::backend::wpd=debug`. [#61](https://github.com/vdavid/mtp-rs/issues/61)
- **[lib] The virtual device can act like a camera as well as an Android phone.** `VirtualDeviceConfig::android` (default `true`) decides how the host stops transfers against it. `true` behaves like Android: it advertises `android.com`, a stopped upload leaves nothing behind, and `UploadError::partial` is `None`. `false` behaves like a PTP camera: the host cancels with CLASS_CANCEL, and a stopped upload keeps its half-written object and surfaces it in `UploadError::partial`. Set it to `false` to test your camera path or your partial-upload cleanup. [#62](https://github.com/vdavid/mtp-rs/issues/62)

## [0.38.0] - 2026-11-06

Library `0.38.0` and CLI `0.11.8`. Downloads read in windows of about two seconds, so a cancel on Android stops within one window instead of reading the rest of the file, and the device stays usable during a big download. Breaking for library users: `download_windowed` is gone; see the first bullet for how to migrate.

### Changed

- **[lib] Breaking: one download API, paced by how long a cancel may take.** `Storage::download` now reads a file in windows that each take about two seconds, sized from the measured speed, and frees the device between them. So a cancel on Android costs at most the current window instead of the rest of the file (a 5 GB file cancelled at 1% over a 2 MB/s link used to hold the device for ~41 minutes), and a folder listing during a big download waits at most about two seconds. Each window costs ~15–20 ms on top of its data on a Pixel 9 Pro XL, about 1% at that size. `Storage::download_with_pacing` takes a `DownloadPacing`: `HeldOpen` (one transfer, the old `download`), `MaxCancelDelay(duration)` (the default, at `DEFAULT_MAX_CANCEL_DELAY`), or `FixedWindow(bytes)`. Windows stream their chunks, so memory stays at one 64 KiB read however big a window gets. Devices without a partial-read operation get one held-open transfer automatically. `FileDownload` gains `offset()`, `modified()`, `created()`, and `holds_session()` (for consumers with their own device lock), and `progress()` now counts from the start of the file, so a resumed download picks up where it left off. **Removed**: `Storage::download_windowed`, `Storage::download_windowed_default`, `WindowedDownload`, and `DEFAULT_DOWNLOAD_WINDOW`. To migrate, use `download_with_pacing(handle, range, DownloadPacing::FixedWindow(size))` and read chunks with `next_chunk()`; the session is free whenever `holds_session()` is `false`. [#60](https://github.com/vdavid/mtp-rs/issues/60)
- **[lib]** On Windows, a download's windows continue on one read stream (like `read_range` already did) and other requests run between them.
- **[cli] Stopping `get` (Ctrl+C) during a big file leaves at most one window behind for the next command to clear, not the rest of the file.** `get` uses the new paced download. The next command clears leftovers when it opens the device, for up to 30 s, so a big leftover could outlast that before.

### Fixed

- **[lib] A finished download no longer blocks every later call on the device until it's dropped.** A `FileDownload` that had returned its last chunk kept the device's session, so any call while it was still in scope waited forever, with no error and no timeout. The session now goes back as soon as the transfer ends. Related: a transfer error mid-download (a timeout, say) now makes the next operation clear what's left of the transfer first, instead of reading it as its reply. [#60](https://github.com/vdavid/mtp-rs/issues/60)

### Added

- **[workspace]** `mtp-bench` gained `stream`, `paced`, and `windowed` download modes (`--windows` sizes in MiB), to measure what windows cost on a device.

## [0.37.3] - 2026-11-04

Library `0.37.3` and CLI `0.11.7`. A copy onto an existing name can't corrupt a file or hand back the original anymore, and cancelling a download no longer kills MTP on Android until a replug.

### Fixed

- **[lib] Copying a file into a folder that already holds its name no longer reports a copy that never happened, or overwrites the existing file.** Android answers such a copy with OK and a handle that names nothing, and writes the source over the existing file without truncating it, so a longer file keeps its old tail. On Windows the library then found the copy by name and returned the original, so an app renaming "the copy" renamed the original. `Storage::copy_object` now lists the destination first and refuses with the new `Error::AlreadyExists` (letter case ignored, since Android's shared storage ignores it). Over USB it also checks the handle the device answered with, and on Windows it returns only an object that wasn't there before. A same-folder copy now fails cleanly: copy into another folder, rename, and move it back. [#59](https://github.com/vdavid/mtp-rs/issues/59)
- **[cli] `cp` makes a renamed copy in the source's own folder, and no longer renames the original.** A device makes a copy under the source's name, and Android can't do that next to the original, so on Windows `cp /a.jpg /b.jpg` renamed `a.jpg` to `b.jpg`. `cp` now copies through a hidden staging folder (copy, rename, move back, remove the folder), and with `--replace` it no longer deletes the destination before a copy that can't happen. Verified on a Pixel 9 Pro XL. [#59](https://github.com/vdavid/mtp-rs/issues/59)
- **[lib]** The virtual device copies the way Android does: an existing destination is overwritten in place, never truncated, and a name clash gets Android's answer. It used to truncate a file copied onto itself to zero bytes.
- **[lib] Cancelling a download no longer kills MTP on Android phones until a replug.** A cancel sent the USB CLASS_CANCEL request, which races Android's MTP server as it cancels its own transfers. About one time in ten the server lost and exited, and only unplugging the phone brought it back (Pixel 9 Pro XL and Galaxy S23 Ultra; the automatic reset didn't help). On Android the library now skips that request and reads the rest of the file instead, which never starts the server's cancel. No wedge in 150+ cancels on each phone. The cost: a cancel takes as long as the rest of the transfer, about 2.5 s per 700 MB on the Pixel, so prefer `download_windowed` for big files. It covers explicit `cancel()` and dropped downloads alike. Cameras keep the CLASS_CANCEL they need. [#54](https://github.com/vdavid/mtp-rs/issues/54)

## [0.37.2] - 2026-11-04

Library `0.37.2` and CLI `0.11.6`. A malformed response from a device can no longer abort the process.

### Fixed

- **[lib] A response container whose length field is shorter than its 12-byte header is now an error, not a crash.** The parameter count subtracted the header from the device-chosen length, so a length below 12 panicked in debug builds and, in release, wrapped to a count that aborted the process on allocation. Any device, or a USB glitch, could send one. Found by the new fuzz targets. The CLI ships with the fix in `0.11.6`.

### Added

- **[workspace]** cargo-fuzz targets for every parser that reads device bytes (containers, `DeviceInfo`, `StorageInfo`, `ObjectInfo`, `GetObjectPropList`, `DevicePropDesc`, property values, strings and arrays, dates, and the SIC device status), with round-trip properties where one applies, a committed seed corpus, and a weekly fuzz workflow. They follow OSS Verify's rules (single-threaded, input only), so the parsers can get machine-checked proofs. The `fuzzing` feature that exposes them is for the fuzz crate only and exempt from SemVer.

### Changed

- **[workspace] The MSRV is now Rust 1.90 (was 1.91).** Both crates build on it as they are, and 1.90 is the toolchain OSS Verify proves fuzz targets with. Lowering it breaks no one, so it ships in a patch.
- **[workspace]** `parking_lot` 0.13 (dev-only) and refreshed lockfile dependencies, `nusb` 0.2.9 among them.

## [0.37.1] - 2026-11-01

Library `0.37.1`. The CLI doesn't use the changed call, so it stays at `0.11.5`.

### Fixed

- **[lib] `ptp::PtpError::is_exclusive_access` now works on Linux, and always agrees with `mtp::Error`.** It searched the error message for "device or resource busy", but nusb's Linux text is "interface is busy (errno 16)", so it answered `false` when another process or a kernel driver held the device. It now goes by nusb's error kind, like `mtp::Error` does. One visible change: `PtpError::Io` always answers `false` now. In the library it only carries transfer faults, and an "access denied" message there is `mtp::Error::PermissionDenied`, which has a different remedy.

### Changed

- **[workspace]** `serial_test` 4 (dev-only), `actions/checkout` 7, and `ubuntu-26.04` for the Homebrew formula job.

## [0.37.0] - 2026-10-31

Library `0.37.0` and CLI `0.11.5`. The API gets small enough to promise: `nusb` is out of it, the wire format is private, and new enum variants and struct fields can land without breaking you. No device behavior changes on any backend. This starts the quiet period toward 1.0.

### Breaking

**[lib]** Most code that uses `MtpDevice` and `Storage` needs only the wildcard arms and the renamed paths below. The stable API is what the crate root re-exports, plus `mtp::` and `ptp::`.

Paths that moved:

| Old | New |
|-----|-----|
| `mtp_rs::register_virtual_device`, `unregister_virtual_device`, `rescan_virtual_device`, `rekey_virtual_object`, `pause_watcher`, `force_operation_wedge`, `force_cancel_wedge`, `force_object_info_error`, `force_partial_read_caps`, `clear_dropped_paths`, `clear_object_info_errors`, `dropped_paths_since_pause`, `was_path_dropped`, `WatcherGuard`, `RescanSummary`, `DROPPED_PATHS_CAP`, `VirtualDeviceConfig`, `VirtualStorageConfig` | The same names under `mtp_rs::virtual_device::` |
| `mtp_rs::transport::virtual_device::config::*` | `mtp_rs::virtual_device::*` |
| `mtp_rs::transport::*` (`Transport`, `NusbTransport`, `UsbDeviceInfo`, `MtpMatchReason`, `BulkStream`) | Same path, with the `unstable-raw` feature. `UsbSpeed` and `MtpMatchReason` are at the root without it |
| `MtpDeviceBuilder::open_nusb_device`, `UsbDeviceInfo::open`, `PtpSession::open`, `PtpSession::open_reusing_existing` | Same names, with the `unstable-raw` feature |
| `NusbTransport::list_mtp_devices()` for enumeration | `MtpDevice::list_devices()`, or the new `PtpDevice::list_devices()` (USB only), then `open_by_serial` / `open_by_location`. Use `MtpDeviceBuilder::known_devices` in place of hand-picking a `nusb::Device` |
| `mtp_rs::PtpError`, `mtp_rs::error::PtpError` | `mtp_rs::ptp::PtpError` |
| `mtp_rs::{EventCode, OperationCode, ResponseCode, ReceiveStream, receive_stream_to_stream}` | The same names under `mtp_rs::ptp::` |
| `mtp_rs::cancel::CancelToken` | `mtp_rs::CancelToken` |

Things that changed shape:

- **`#[non_exhaustive]` on most public types.** Add a `_` arm where you match `DeviceEvent`, `UsbSpeed`, `Backend`, `ByteRange`, `StorageType`, `FilesystemType`, `PtpError`, or any `ptp::` enum. Struct literals of `DateTime`, `ptp::ObjectInfo`, and `NewObjectInfo` don't compile anymore: use `DateTime::new` or `from_unix_seconds`, `ptp::ObjectInfo::default()` plus field assignment, and `NewObjectInfo::file`, `folder`, or `with_format`. `ListingItem` and `HotplugEvent` stay exhaustive on purpose: a new variant should be a compile error, so you decide what it means. The types you only read (`ObjectInfo`, `DeviceInfo`, `StorageInfo`, `Capabilities`, `Progress`, `UploadError`, `SkippedObject`, `ObjectCollection`, and the `ptp::` structs) can't be built or destructured without `..` from outside anymore. `VirtualDeviceConfig { .., ..Default::default() }` keeps working.
- **`PtpError::Usb` holds an opaque `ptp::UsbError`**, and `From<nusb::Error> for PtpError` is gone. Read it through `Display`, or convert to `mtp_rs::Error`, which sorts USB failures into `ExclusiveAccess`, `PermissionDenied`, `Disconnected`, and so on.
- **The `ptp::` wire format is private**: `pack_*`, `unpack_*`, `CommandContainer`, `DataContainer`, `ContainerType`, `container_type`, `TransactionId`, and `from_bytes` / `to_bytes` on `DeviceInfo`, `StorageInfo`, `ObjectInfo`, `DevicePropDesc`, `PropertyRange`, `ResponseContainer`, and `EventContainer`, plus `PropertyFormType::{from_code, to_code}`. `PropertyValue::{to_bytes, from_bytes}` and `ObjectPropListElement::list_from_bytes` stay, since the raw-bytes calls need them. Tell us if you relied on one of the others.
- **`DeviceEvent::from_container` is gone from the public API**, and `PtpUploadError` is removed (nothing produced it).
- **`mtp_rs::cancel` and `mtp_rs::error` aren't public modules anymore.**

### Added

- **[lib] `unstable-raw` feature**, for when you enumerate with `nusb` yourself or bring your own transport. Exempt from SemVer, so a `nusb` bump can't force an mtp-rs major anymore.
- **[lib] `PtpDevice::list_devices()`**: enumerate at the PTP level with no raw types. Returns `MtpDeviceInfo`, skips virtual devices.
- **[lib] Root re-exports** of `MtpDeviceInfo`, `MtpMatchReason`, `StorageType`, `FilesystemType`, `watch_devices`, `DeviceWatch`, `DeviceWatchBuilder`, `HotplugEvent`, and `DEFAULT_SETTLE_DELAY`, so every stable type has a permanent root path.
- **[workspace] `SECURITY.md`**, and a "Stability" section in the README: SemVer scope, patch releases for pure fixes, MSRV, and security fixes. MSRV is unchanged (Rust 1.91), and a raise counts as a minor bump with a changelog line, not a SemVer break.
- **[workspace] `cargo-semver-checks`** in `just check-all` and CI, to catch accidental breaks.

### Changed

- **[lib] `receive_stream_to_stream` promises `Send`** in its signature, where it used to leak through `impl Trait`.
- **[cli]** `mtp-rs reset` lists devices through `PtpDevice::list_devices()`. Same behavior, and the CLI needs no `unstable-raw`.

## [0.36.3] - 2026-10-30

Library `0.36.3` and CLI `0.11.4`. One broken file name can't fail a whole folder listing anymore.

### Fixed

- **[lib] A file name with a broken emoji no longer fails the whole listing.** A device that cuts a long name in the middle of a UTF-16 surrogate pair sent us text we refused to decode, which failed the object's info and with it the folder. The broken character now reads as U+FFFD (`�`) and the rest of the name and folder come through. Found in code in [#50](https://github.com/vdavid/mtp-rs/issues/50), not seen on hardware.

## [0.36.2] - 2026-10-30

Library `0.36.2` and CLI `0.11.3`. A cancel on a device that has stopped answering now tells you to reopen.

### Fixed

- **[lib] A cancel on a device that has stopped answering now returns `Error::DeviceReset`**, where it could return `Timeout` after 0.3 s with no drain and no reset, leaving the caller with a dead session and no hint to reopen. When the USB cancel request itself times out, the cancel now continues like a refused one, and the status check after the drains decides whether the device is wedged. Seen on a Galaxy A57 on Linux in [#43](https://github.com/vdavid/mtp-rs/issues/43).

## [0.36.1] - 2026-10-30

Library `0.36.1` and CLI `0.11.2`. A USB cable pulled mid-transfer on macOS reads as a disconnect instead of an unknown I/O error.

### Fixed

- **[lib] Pulling the USB cable mid-transfer on macOS now reads as `Disconnected`**, where the first error after the pull used to arrive as `Io("unknown (error 0xe00002ed)")`. macOS fails the in-flight transfer with `kIOReturnNotResponding` and the next one with `kIOReturnNotOpen`, and nusb maps neither to a disconnect, so apps showed a generic I/O failure for a pulled cable. Both codes now map to `PtpError::Disconnected` (and `Error::Disconnected`), including during a cancel's drain. Reported in [#53](https://github.com/vdavid/mtp-rs/issues/53).
- **[cli] A cable pulled mid-command on macOS says "device disconnected"**, where it used to print a raw IOKit error code.

## [0.36.0] - 2026-10-28

Library `0.36.0` and CLI `0.11.1`. A file or folder name longer than MTP allows is refused up front instead of garbling the transfer.

### Fixed

- **[lib] A file or folder name longer than MTP allows is refused instead of garbling the transfer.** PTP strings hold at most 254 UTF-16 code units (an emoji is two), but `pack_string` wrote the length into one byte with a silent wrap, so a 255-unit name told the device the name was empty and it misparsed everything after it. `Storage::upload`, `create_folder`, and `rename` now return the new `Error::NameTooLong { units }` before anything is sent, so nothing is created or changed and the session stays usable. `MAX_NAME_UNITS` (crate root and `mtp::`) is the limit to check against. The CLI reports it as a remote path error (exit code 5). Reported in [#49](https://github.com/vdavid/mtp-rs/issues/49).
- **[lib] The virtual device lists a backing file whose name is too long for MTP**, cutting it at 254 units the way Android does, where it used to send a wrapped length byte.
- **[cli] `put`, `mkdir`, and `rename` with a name over 254 UTF-16 units exit with code 5 (remote path error)** and a message naming the limit, where the device used to receive a garbled name.

### Changed

- **Breaking:** **[lib]** `ptp::pack_string`, `PropertyValue::to_bytes`, and `PropertyRange::to_bytes` return `Result<Vec<u8>, PtpError>`, failing with the new `PtpError::StringTooLong { units }` past `ptp::MAX_STRING_UNITS` (254). `PtpSession::rename_object` and `set_device_prop_value_typed` surface it before sending the command. Code that matches `PtpError` exhaustively needs an arm for the new variant.

## [0.35.0] - 2026-10-27

Library `0.35.0` and CLI `0.11.0`. Both move to a rolling 12-month MSRV, now Rust 1.91.

### Changed

- **Breaking:** **[workspace]** The MSRV is now Rust 1.91 (was 1.85), under a new policy: both crates support the Rust releases from the last 12 months, and raising the MSRV is always a minor version bump with a changelog line. When a dependency's newest release needs a newer Rust than yours, Cargo's MSRV-aware resolver (`resolver = "3"`, the edition 2024 default) picks one you can build.

## [0.34.3] - 2026-10-24

Library `0.34.3` only; the CLI stays at `0.10.0`. A process killed mid-download no longer wedges the phone.

### Fixed

- **[lib] A process killed mid-download no longer wedges the phone until a replug.** The phone keeps sending the rest of the interrupted transfer, so the next process's open read file bytes as its reply and failed with `expected Response container type (3), got N` (a different N every run), and so did every process after it. Opening a device now recognizes that, drains the leftover data, and asks again. A killed windowed download costs the next open under a second; a killed held-open `download` of a large file can still leave more than the 30-second drain clears. Reported by [@IoSonoPiero](https://github.com/IoSonoPiero) in [#43](https://github.com/vdavid/mtp-rs/issues/43).

## [0.34.2] - 2026-10-24

Library `0.34.2` only; the CLI stays at `0.10.0`. Cancelling a download works on phones that refuse the USB cancel request.

### Fixed

- **[lib] Cancelling a download no longer wedges a phone that refuses the USB cancel request.** The Xiaomi POCO X6 Pro answers CLASS_CANCEL with a STALL on Linux, and `FileDownload::cancel` returned that as `Error::Io("endpoint stalled")` with the rest of the file still in the pipe, so the next operation read file bytes as its response and the phone stayed stuck until a replug. The cancel now drains the rest of the transfer instead (it costs the remaining bytes, but the session stays usable), and a cancel that fails for any other reason makes the next operation drain the pipe first. A late reply to an earlier transaction is now skipped too, so the cancelled download's closing reply, arriving after the cancel finished, no longer derails the next command ("Transaction ID mismatch: expected 119, got 118"). Reported by [@IoSonoPiero](https://github.com/IoSonoPiero) in [#43](https://github.com/vdavid/mtp-rs/issues/43).

## [0.34.1] - 2026-10-23

Library `0.34.1` only; the CLI stays at `0.10.0`. Big files copy at full speed on Windows.

### Fixed

- **[lib] Windowed downloads on Windows no longer slow down as the file goes on.** The WPD backend reopened the object's stream for every window, and on devices whose stream can't seek (`IStream::Seek` is `E_NOTIMPL`) it re-read everything before the window's offset, so a large file took time quadratic in its size. On a Xiaomi POCO X7 Pro, a 194 MB video in 4 MiB windows fell below 1 MB/s by 128 MB; now 186 files (1.04 GB, including two videos of 180 and 194 MB) copy in 39 s, about 26 MB/s, byte-identical to a copy made with File Explorer. The worker keeps the stream of the last window and continues on it when the next window starts where it stopped. Any other request releases it first, since an open stream keeps the device busy, and so does a second with no request, so a paused copy doesn't hold the phone. Contributed by [@IoSonoPiero](https://github.com/IoSonoPiero) in [#46](https://github.com/vdavid/mtp-rs/pull/46).

## [0.34.0] - 2026-10-23

Library `0.34.0`, CLI `0.10.0`. Big folders list 10x+ faster on Android: a Galaxy S23 Ultra lists 3,248 photos in 0.4 s instead of 5.5 s.

### Changed

- **[lib] Folder listings take six requests instead of one per object, when the device advertises `GetObjectPropList` (0x9805).** On a Galaxy S23 Ultra, a folder of 3,248 photos lists in 0.40 s instead of 5.5 s, and 1,916 screenshots in 0.18 s instead of 2.6 s; on a Pixel 9 Pro XL, 274 photos list in 0.24 s instead of 2.2–2.8 s. Names, sizes, formats, parents, and modification dates match the old path exactly. `list_objects` and friends used to send one `GetObjectInfo` per child, which is where large Android folders spent their seconds: Android opens every photo to read its EXIF data for that call. Over USB the backend now asks `GetObjectPropList` for the folder's children (depth 1), one request per property it needs (six in all). It never asks for every property at once: Android builds that answer from the first child's file type, and on a Pixel 9 Pro XL a folder of 726 screenshots spent 3.2 s on it before the phone declined, against about 0.25 s for the six single-property requests. On a Xiaomi POCO X7 Pro (HyperOS 3, Android 16) a tree of 1,938 objects lists in 0.67 s instead of 7.3 s with identical names, sizes, formats, parents and dates. Sizes come from the 64-bit `ObjectSize`, so files over 4 GB need no extra lookup there. Root listings and devices without the operation keep the per-handle path. Contributed by [@IoSonoPiero](https://github.com/IoSonoPiero) in [#45](https://github.com/vdavid/mtp-rs/pull/45).
  - **It never shows a wrong folder.** An answer that leaves out a name, format, size, or parent for any child, or names an object from another folder, falls back to the per-handle path. A device that can't serve the operation at all stops being asked for the rest of the session; a refusal about one folder (like a stale handle) falls back for that listing only. Timeouts, disconnects, and resets still surface as errors.
  - **Two fields differ on this path.** `ObjectInfo::image_width` and `image_height` stay 0, since only `GetObjectInfo` carries them. On Android, `created` is `None`: its `GetObjectInfo` reported the modification time there, and its property list has no creation date.
  - **Turn it off** with `MtpDeviceBuilder::use_object_prop_list(false)` if a device lists wrongly.
  - New low-level API: `PtpSession::get_object_prop_list`, `ptp::ObjectPropListElement::list_from_bytes`, and `ptp::ObjectInfo::set_property`. The Windows (WPD) backend is unchanged.
- **[lib] The virtual device serves `GetObjectPropList`, modeled on Android**, so tests of a consumer's own code exercise the fast listing path by default. New `VirtualDeviceConfig` fields: `supports_object_prop_list` (default `true`) and `prop_list_declined_properties`.

### Fixed

- **[lib] The virtual device refuses a stale folder handle instead of listing it as empty.** Listing a folder by a handle the device no longer knows (for example after `rekey_virtual_object`) returned an empty folder; it now fails with `Error::StaleHandle`, like Android does (`InvalidObjectHandle`), so tests of a consumer's stale-handle recovery see what a phone would send.

### Added

- **[cli] `--no-prop-list` lists folders one object at a time**, for a device whose listings look wrong, or to compare the two.
- **[lib] `examples/list_speed.rs` times a folder listing both ways and prints any field that differs**, to check a new device.

## [0.33.2] - 2026-10-22

Library `0.33.2` only; the CLI stays at `0.9.1`. Windows reports the same errors as USB for the same device answer.

### Fixed

- **[lib] On Windows, a stale or deleted object's handle reports `Error::StaleHandle`, like it does over USB.** The Windows MTP driver passes a device's MTP response code back inside an `HRESULT` (`0x8004_xxxx`), and the WPD backend turned those into `Error::Other`. It now maps them the same way the USB backend maps the raw codes, so `InvalidObjectHandle` is `StaleHandle`, `StoreFull` is `StorageFull`, `DeviceBusy` is `Busy`, and so on. Found by [@IoSonoPiero](https://github.com/IoSonoPiero) on a Xiaomi POCO X7 Pro ([#41](https://github.com/vdavid/mtp-rs/pull/41)).

## [0.33.1] - 2026-10-21

Library `0.33.1` only; the CLI stays at `0.9.1`, since it doesn't print dates. Files listed on Windows have dates now.

### Fixed

- **[lib] The Windows (WPD) backend reports object dates.** `ObjectInfo::created` and `ObjectInfo::modified` were always `None` on Windows, so listings had no dates there while the USB backend had them. They now come from `WPD_OBJECT_DATE_CREATED` and `WPD_OBJECT_DATE_MODIFIED` (`VT_DATE`), as zoneless values like the ones Android writes over USB: on a Xiaomi phone they match the time in the camera's file names. A device that doesn't report a date still yields `None`. `WindowedDownload::modified()` and `created()` pick these up too. Contributed by [@IoSonoPiero](https://github.com/IoSonoPiero) in [#41](https://github.com/vdavid/mtp-rs/pull/41).

## [0.33.0] - 2026-10-07

Library `0.33.0`, CLI `0.9.1`. Copies can keep their dates: a windowed download hands back the file's modification time for free, dates say honestly whether they carry a time zone, and the virtual device does dates in both directions, so all of it is testable without a phone.

### Added

- **[lib] `WindowedDownload::modified()` and `WindowedDownload::created()`.** `download_windowed` already fetches the object's `ObjectInfo` to learn its size; it now keeps the dates from it too, so a copy that keeps the source's date no longer needs a second `GetObjectInfo` per file ([#36](https://github.com/vdavid/mtp-rs/issues/36)).
- **[lib] `DateTime` knows whether the device gave a time zone.** New `offset: Option<UtcOffset>` field on both `mtp::DateTime` and `ptp::DateTime`, filled from a `Z` or `±hhmm` suffix. Most devices write none: Android writes the phone's local time without a zone, and `None` means exactly that, never UTC. New methods: `to_unix_seconds()` (answers only when there's an offset), `to_unix_seconds_with_fallback(zone)` (you pick the zone for a zoneless value), `from_unix_seconds(secs, offset)`, `with_offset(offset)`, and `mtp::DateTime::new`. `UtcOffset` is exported from the crate root, `mtp::`, and `ptp::`. Values with an offset are written with it, so `from_unix_seconds(secs, UtcOffset::UTC)` sends `…Z`, which Android reads correctly (it ignores any other offset and assumes phone-local time).
- **[lib] The virtual device has dates.** Objects report their backing file's modification time as `DateModified` (and its birth time as `DateCreated` where the filesystem records one), and an upload's `DateModified` becomes the file's modification time, the way Android stamps it. A test seeds a date with `File::set_modified` on the backing file. New `VirtualDeviceConfig` fields: `utc_offset` (the device clock's zone, default UTC) and `dates_include_offset` (default `false`: zoneless, like Android). Both have defaults, so configs built with `..Default::default()` keep compiling.

### Changed

- **[lib] Breaking: `DateTime` gained a public `offset` field**, so a struct literal needs `offset: None` (or use `DateTime::new`). Applies to `mtp::DateTime` and `ptp::DateTime`.
- **[lib] Datetime parsing is exact.** Month lengths and leap years are real, so `20230229T…` and `20240431T…` no longer parse, and `DateTime::new`/`is_valid` reject them too. Numeric fields must be digits (a `+` sign used to slip through), tenths of a second (`.s`) are accepted and dropped, and a string with any other trailing text yields `None`: an unrecognized suffix might be an offset, and reporting the value as zoneless would shift it silently. As before, an unparseable date in an `ObjectInfo` reads as "no date" without failing the listing.
- **[cli] Built on `mtp-rs` 0.33.0.** No behavior change.

## [mtp-rs-cli 0.9.0] - 2026-09-12

CLI `0.9.0` only; the library stays at `0.32.0`. `get` copies whole folders, so pulling a phone's camera roll is one command.

### Added

- **[cli] `get` downloads whole folders.** When the remote path is a folder (the storage root included), `get` copies it and everything below it, empty subfolders too, into the local path. `--replace` merges into an existing local directory, replacing files the device also has. Objects the device won't describe are reported like `ls` does: a stderr warning and a `skipped` array in `--json`. JSON output from `get` now carries a `kind` field (`"file"` or `"folder"`). Contributed by [@max619](https://github.com/max619) in [#32](https://github.com/vdavid/mtp-rs/pull/32).
- **[cli] A folder download fails up front, before writing anything, when the copy couldn't be faithful.** The tree is listed first, with a running count on stderr (a phone camera roll takes minutes to list). A device-supplied name that can't exist locally stops the command: `..` and separators everywhere, plus Windows' forbidden characters, trailing dots and spaces, and reserved names like `CON` on Windows. So do two names that would land on the same local file, like `IMG.jpg` and `img.JPG`: `get` asks the destination disk whether case matters, since a case-sensitive Linux machine can still be writing to a case-insensitive FAT or exFAT card. Without this, the copy failed halfway, or with `--replace` silently kept only one of the two files.

## [0.32.0] - 2026-08-28

Library `0.32.0`, CLI `0.8.2`. A device that keeps its MTP session alive across host processes can now be driven without knocking it out of MTP mode, and both published crates ship their license texts.

### Added

- **[lib] Reuse an existing device-side PTP session instead of closing and reopening it.** New `MtpDeviceBuilder::reuse_existing_session(id)` (and the low-level `PtpSession::open_reusing_existing`) opt in to a stable session ID that survives across host processes: when `OpenSession` reports `SessionAlreadyOpen`, the open path keeps the existing session rather than sending `CloseSession`. This is USB-only (ignored when WPD is selected on Windows) and opt-in, so the default close-and-reopen behavior is unchanged. It's what a one-process-per-command client needs on hardware like the Teenage Engineering TP-7, which treats `CloseSession` as a request to leave MTP mode. Hardware-verified by A/B on a TP-7 (firmware 2.5.7 / MTP 1.1.11). Thanks to [@worm-emoji](https://github.com/worm-emoji) (#29).

### Fixed

- **[workspace] Both published crates now carry their license texts.** `LICENSE-MIT` and `LICENSE-APACHE` lived only at the repo root, outside either package, so neither crates.io tarball contained them. MIT's notice has to travel with the software, so this was a compliance gap. Each crate directory now has its own copy.

## [0.31.0] - 2026-08-27

Library `0.31.0`, CLI `0.8.1`. Some MTP responders can only hand over whole objects, and until now a consumer's fallback for them could only be tested on hardware. A virtual device can model one now.

### Added

- **[lib] A virtual device can now model a responder that only hands over whole objects.** `VirtualDeviceConfig` gained `supports_partial_object` (feature `virtual-device`), the 32-bit `GetPartialObject` twin of the existing `supports_partial_object_64`. Set both to `false` and `Capabilities::supports_partial_download` reports `false`, every ranged read fails `Error::Unsupported`, and `download(handle, ByteRange::Full)` is the only way to the bytes. That's the shape of libhaze (Sphaira on the Nintendo Switch) and of simple PTP responders, and it's what a consumer's whole-object fallback path needs in order to be testable without that hardware. Defaults to `true`, so existing configs built with `..Default::default()` behave exactly as before.

### Changed

- **[lib] A virtual device now refuses a partial-read operation it doesn't advertise**, answering `Operation_Not_Supported` instead of serving it anyway. Previously `supports_partial_object_64: false` only removed the op from `OperationsSupported` while the dispatch still handled it, so a consumer that ignored the capability flag and called the operation regardless passed its tests and failed on hardware. Only affects a virtual device configured with a partial-read op turned off.

## [0.30.0] - 2026-08-10

Library `0.30.0`, CLI `0.8.0`. Two independently-written Nintendo Switch MTP responders turned up three things at once: a listing that threw away 49 good files because of one bad one, a root filter that discarded every entry, and, underneath both, a root-parent value we'd been sending wrong since the beginning. That last one was never a Switch problem. It's why creating a file or folder in an Android phone's storage root has never worked, which these docs blamed on Android for months.

### Fixed

- **[lib] One rejected `GetObjectInfo` no longer hides every valid sibling in a directory.** A device can hand back a folder's handle list and then refuse to describe one of them (Sphaira, the Nintendo Switch homebrew responder, does this for one handle out of 50). That used to fail the whole listing, so a folder with 50 files read as an error instead of 49 files. Now `Storage::list_objects` keeps the readable siblings, and `Storage::collect_objects` additionally reports which handle it couldn't read, so you can tell "49 files" from "49 files and a 50th we couldn't see". `collect_objects_recursive` aggregates that across a whole tree walk. Reported and hardware-tested by [@oenderg](https://github.com/oenderg) against two independent responders ([#22](https://github.com/vdavid/mtp-rs/issues/22), [#23](https://github.com/vdavid/mtp-rs/pull/23)).

  A per-handle failure is skipped only when the handle list is already in hand, the failing operation is read-only, and the device answered with a protocol response code that closed the transaction cleanly. Exactly one case qualifies today (`GeneralError` on `GetObjectInfo`). Transport and session failures, malformed responses, cancellation, stale handles, and any failure to enumerate the handles at all stay fatal. So does a folder where *every* handle failed: a device that answered nothing is broken, and reporting it as an empty folder would read as "everything was deleted" to anything syncing.

  `mtp-rs ls` warns on stderr (in both plain and `--json` modes) and always emits a `skipped` array; `mtp-rs doctor` reports `unreadable_root_objects`.
- **[lib] Root listings work on responders that report the containing storage ID as the parent of root objects.** DBI and Sphaira/libhaze (Nintendo Switch homebrew) do this for every root entry, so the root filter discarded all of them and a Switch listed as empty. The filter now accepts the storage ID as a root marker, but only when that value isn't itself one of the handles the device enumerated: on the recursive fallback path a device with a large library can genuinely own a folder at that handle (`0x00010001` is only 65,537), and treating it as "root" would list that folder's children among the real root entries. Thanks to [@oenderg](https://github.com/oenderg) for finding this and for testing two independent responders ([#20](https://github.com/vdavid/mtp-rs/pull/20)).
- **[lib] Uploading a file or creating a folder in a storage root works, instead of failing with `InvalidObjectHandle`.** `SendObjectInfo`'s parent parameter is the one place MTP spells the root as `0xFFFFFFFF` (MTP 1.1 D.2.12); we were sending `0`, which means "no parent given, responder may choose", so responders looked it up as a handle and rejected it. Android's `MtpServer` maps only `MTP_PARENT_ROOT` (0xFFFFFFFF) to the storage path, and libhaze (the Nintendo Switch homebrew responder behind Sphaira, [#21](https://github.com/vdavid/mtp-rs/issues/21)) only maps 0xFFFFFFFF to its storage object; both answer `0` with `InvalidObjectHandle`. What we documented as an Android limitation ("uploads to the storage root are rejected, use `Download/` instead") was this bug. `parent = None` on `Storage::upload` / `create_folder` now addresses the root. `MoveObject`/`CopyObject` are untouched: the spec asymmetrically spells *their* root destination `0x00000000`. Verified by A/B on a Pixel 9 Pro XL (macOS/nusb, 2026-08-10): the pre-fix build failed both a root `mkdir` and a root upload, the fixed build did both and verified the readback.

- **[cli] `ls` and `doctor` report what the device wouldn't let them read, instead of quietly coming up short.** `mtp-rs ls` prints a warning on stderr (in both plain and `--json` modes, so stdout stays clean for pipes) and always emits a `skipped` array, empty when everything was readable, so a script can read one field without checking whether the key exists. `mtp-rs doctor` reports `unreadable_root_objects` per storage. Both fields are present on a healthy device too (verified on a Pixel 9 Pro XL), so consumers don't have to handle a sometimes-missing key.

### Changed

- **[lib] Breaking: `ObjectListing::next` now yields `Result<ListingItem, Error>`.** `ListingItem` is `Object(ObjectInfo)` or `Skipped(SkippedObject)`, so `Err` keeps a single meaning (the listing is over) and an unreadable object can't be mistaken for a fatal error. Every existing consumer gets a compile error rather than a silent behavior change, which is the point: each one has to decide what a skipped object means for it. If the answer is "ignore it", `ListingItem::object()` returns an `Option<ObjectInfo>`, or match on `ListingItem::Object(info)` and drop the rest.
- **[lib] `VirtualDeviceConfig` gained `undescribable_objects`** (storage-relative paths whose `GetObjectInfo` fails while the object stays present), and the registry gained `force_object_info_error` / `clear_object_info_errors`. Together they let a consumer test how it renders a partially-readable folder with no hardware, in process or out of it. Defaults to empty, so existing configs built with `..Default::default()` are unaffected.
- **[workspace] Corrected recovery guidance: the transport reset is a last resort on Android, not step two.** The reset APIs (`MtpDevice::reset_by_serial` / `reset_by_location` / `reset_first`, the `MtpDeviceBuilder` forms, `PtpDevice::reset_device`), the `mtp-rs reset` CLI help, and `docs/debugging.md` all documented drop → reset → quiet pause → spaced reopen as *the* way to recover a wedged device. On Android, the most common MTP device class, that advice can break the phone. Sending the SIC `DEVICE_RESET` (0x66) to a **healthy** Pixel 9 Pro XL killed its MTP function outright: Android's `MtpServer` lost its FunctionFS endpoint read (`ECANCELED`, then `EPIPE`) and never re-armed, while the USB device controller stayed `configured`, so the phone kept enumerating, kept appearing in `mtp-rs devices`, and answered nothing. Ten spaced reopens over about 100 s all timed out, and only a physical replug recovered it (verified on a Pixel 9 Pro XL, macOS/nusb + `adb logcat`, 2026-07-21). The docs now put quiet spaced reopens first (enough on their own for a Pixel) and the reset last, for a device that's already unreachable. No behavior changed; if you built recovery logic on the old advice, reorder it.
- **[workspace] The cancel/abandon wedge is documented as an Android problem, not a Samsung one, and its signature differs by device.** A Pixel 9 Pro XL wedges from the same trigger (a dropped in-flight future, verified by a clean A/B on a Pixel 9 Pro XL, macOS/nusb, 2026-07-20), but where a Samsung surfaces `Error::DeviceReset` on the next operation, a Pixel simply **hangs** with no error at all. Consumer code watching only for `DeviceReset` never notices the Pixel case, so it needs a timeout around operations too. A Pixel's wedge also self-heals on a fresh open. New note with the full evidence, the mechanism, and which claims rest on a single observation: `docs/notes/android-wedges-and-the-reset-kill-switch.md`.

## [0.29.0] - 2026-07-21

Library `0.29.0`, CLI `0.7.4`. Recovering a wedged device stops being a CLI-only trick: consumers get the session-less reset, the error that says a reset happened now reaches them, and the diagnostic that captures the evidence finally runs on Android.

### Fixed

- **[lib] A root listing on a sick device now reports what actually went wrong.** The root-listing fast path asks `GetObjectHandles(parent=0xFFFFFFFF)` first and falls back to `parent=0` when the device declines, but it treated *every* failure as a decline. So a session that had just died took a second doomed roundtrip (the #18 notes say hammering a wedged device re-wedges it into a hard `Timeout`), and the caller got the second attempt's error while the first was thrown away. A consumer watching for `Error::DeviceReset` to drive a quiet reopen never saw it, on what is often the first call after a device goes sour. Now only a genuine decline falls back: a `Protocol` response code, or `Io`, which is how a bulk STALL arrives and how SIC-compliant cameras signal an unsupported operation ([#12](https://github.com/vdavid/mtp-rs/issues/12)). `DeviceReset`, `Timeout`, `Disconnected`, `Cancelled`, and a desynced session propagate untouched.
- **[cli] `doctor --probe-cancel` now runs on Android phones, where it used to skip.** The probe looked for a file at the storage root, but an Android MTP root is the top of shared storage and by convention holds only directories (`DCIM`, `Download`, `Pictures`, `Android`, …). A clean phone has zero files there, so the probe reported `skipped (no file at storage root to probe)` on exactly the devices [#18](https://github.com/vdavid/mtp-rs/issues/18) is about (verified on a Pixel 9 Pro XL: 17 directories, zero files; a Galaxy S23 Ultra only worked by luck, on a stray 36-byte file). It now walks breadth-first from the root, bounded to 48 folders and three levels deep, visiting user folders before the huge `Android` tree. When it still finds nothing, the message says how far it looked. A `--probe-path <PATH>` override pins a specific file. Verified on a Pixel 9 Pro XL: the same phone that reported `skipped` now reports `healthy (cancelled '/Pictures/…' (115538 bytes); session survived)`.
- **[cli] The probe no longer insists on a big file.** It prefers a 100 KB-10 MB file and stops at the first one, but falls back to any file rather than skipping. Transfer size doesn't drive the wedge: `--probe-cancel` reported `wedged_recovered` on a 36-byte file (verified on a Galaxy S23 Ultra SM-S918B, macOS/nusb, 2026-07-20).

### Added

- **[lib] A transport-level device reset on the neutral MTP API: `MtpDevice::reset_by_serial`, `reset_by_location`, `reset_first`** (plus the `MtpDeviceBuilder` forms, which honor `timeout` and `known_devices`; a non-standard-descriptor device is exactly the kind that needs resetting). Recovering a wedged device in software was previously reachable only through the PTP layer (`PtpDevice::reset_device`) or the CLI's `mtp-rs reset`, so a consumer of the neutral API had no way to do the reset step of the #18 recovery and was left with drop-and-reopen alone. It's a free-standing selector rather than a method on an open device on purpose: it claims the USB interface and stops, where the regular opens run `OpenSession` + `GetDeviceInfo`, which is precisely what a wedged device can't answer. Drop your device before calling it (holding it keeps the interface claimed) and reopen after, since the session is gone regardless. Virtual devices return `Error::Unsupported`: a filesystem has no USB transport to reset. The docs carry the whole recovery recipe, including that the first reopens are expected to fail with `Timeout` then `SessionAlreadyOpen` before one succeeds (verified on a Galaxy S23 Ultra SM-S918B, macOS/nusb, 2026-07-20).
- **[cli] `doctor --probe-path <PATH>`.** Pins the file the cancel probe uses (`--probe-path /DCIM/Camera/IMG_0001.jpg`) and implies `--probe-cancel`. For when the search picks a file you'd rather leave alone, or when it finds none.
- **[lib] `force_operation_wedge(serial)` virtual-device test hook** (feature `virtual-device`). Arms a one-shot so the next PTP **operation** fails with `Error::DeviceReset`. The sibling of `force_cancel_wedge`, which only fires for a consumer that calls `cancel()`. A consumer that never cancels still meets `DeviceReset`: it drops an operation future, and the next operation's recovery drain hits the wedged device (verified on a Galaxy S23 Ultra SM-S918B, macOS/nusb, 2026-07-20: a windowed `GetPartialObject64` future dropped after 25 ms produced `DeviceReset` from the drain, and the session was dead afterwards). This hook is how to test the reopen-and-retry response with no hardware. Its first catch was the root-listing fallback bug above.

### Changed

- **[docs] The #18 wedge is no longer described as a "large-backlog cancel".** Backlog size isn't the trigger; interrupting an in-flight bulk read is, at any size. Hardware evidence from a Galaxy S23 Ultra SM-S918B (macOS/nusb, 2026-07-20) also splits the wedge in two: an explicit `cancel()` or a dropped **windowed** read recovers in software, but it needs **spaced** retries (transport reset, then reopens returning `Timeout`, then `SessionAlreadyOpen`, then success), while a dropped held-open **streaming** `GetObject` didn't recover at all and needed a physical replug. And `download_windowed` removes the need to cancel, not the wedge: its recovery drain still wedges the device.

## [0.28.0] - 2026-07-20

Library `0.28.0`, CLI `0.7.3`. Big files stop being the case that breaks: a download costs about 64 KiB of memory whatever its size, and objects past 4 GiB work at all.

### Fixed

- **[lib] A streaming download no longer holds the whole file in RAM.** `Storage::download` presented itself as a stream but its internal buffer grew to the object's full size, so pulling a 4 GB video off a phone peaked around 4 GB of RSS even when the consumer wrote every chunk straight to disk and dropped it. Chunks now come off the front of a `BytesMut`, which reclaims the space as it hands them out: peak memory per transfer is about one 64 KiB USB read, whatever the file's size. `download_to_vec` and `FileDownload::collect` still buffer by design, but they now hold one copy instead of two.
- **[lib] Downloads past 4 GiB finish, and finish clean.** An object too big for a 32-bit `ContainerLength` arrives with the `0xFFFFFFFF` sentinel (MTP 1.1 appendix H.1), which the receive path read as a literal byte count: the transfer never found its end, the response container's 12 bytes were handed to the consumer as file data, and the read then errored out. The stream now ends such a transfer on the resolved object size where the device reports one, and on the USB short packet otherwise. A zero-length packet terminating a data phase that exactly fills a read is tolerated too, instead of surfacing as "Empty response from device".

  Verified on a Pixel 9 Pro XL over SuperSpeed USB with a 4,831,838,208-byte file, round-tripped through `mtp-rs put` and `mtp-rs get`: byte-identical (SHA-256 match), 25.6 s, peak RSS 9.8 MB. The same download on `0.27.0` failed outright with `invalid container type: 53875` after peaking at 4.01 GB of RSS, and left the device wedged until it was replugged.

### Changed

- **[lib] A malformed data container is now rejected instead of silently mis-sliced.** A container declaring a length shorter than its own 12-byte header used to desync the receive stream: the payload window came out empty, and the short length was drained off the front, so everything after it was misread. It now fails fast with an `invalid data` error naming the bad length. No well-behaved device sends one, so this should only ever surface against a broken responder, but it surfaces as a clear error rather than as corrupt bytes.

### Added

- **[lib] `PtpSession::execute_with_receive_stream_sized`.** Takes the payload length the caller already knows, which is what lets a transfer over 4 GiB end on a byte count rather than on short-packet detection alone. `execute_with_receive_stream` is unchanged and still the right call for everything else.
- **[lib] Hardware coverage for the >4 GiB download path.** `test_big_file_over_4gib_round_trip` uploads a generated 4 GiB + 64 MiB payload to a real device and verifies every byte on the way back, so the `0xFFFFFFFF` container-length sentinel is now proven against a real responder and not only against the in-process transport the unit tests drive. Nothing touches local disk on either side, and the object is deleted even when an assertion fails mid-run. It's double-gated behind `#[ignore]` and `MTP_TEST_BIG_FILE=1` so a normal `--ignored` sweep never writes gigabytes to someone's phone.
- **[lib] `Error::is_disconnected()`.** The check a long-lived consumer makes most often (tear down the mount, drop the device from the sidebar), alongside the existing `is_retryable` / `is_exclusive_access` / `is_permission_denied` / `is_stale_handle` predicates. Deliberately false for `Error::DeviceReset`, where the device is still plugged in and reopenable and only the session died.
- **[cli]** No CLI changes; `0.7.3` just tracks the new library version.

## [0.27.0] - 2026-07-20

Library `0.27.0`, CLI `0.7.2`. Consumers can react to a device being plugged in instead of re-listing to find out.

### Added

- **[lib] Watch for devices being plugged in and unplugged: `mtp::watch_devices()`.** Returns a `DeviceWatch` stream of `HotplugEvent::Arrived(MtpDeviceInfo)` / `Left(MtpDeviceInfo)`, so a consumer reacts to a phone appearing instead of re-listing on a timer or on every USB event the system produces. Devices already connected are reported as `Arrived` on first poll, which means one code path and no gap for a device plugged in during startup to fall through. `Left` carries the full device info, including the serial the OS doesn't report on disconnect, so a consumer with two phones attached can tell which one went away. Non-MTP USB traffic never reaches the consumer. Tune with `DeviceWatchBuilder` (`known_devices`, `settle_delay`; default `DEFAULT_SETTLE_DELAY` is 500 ms). USB only, so virtual devices don't produce events. See `examples/watch_devices.rs`. Verified on a Pixel 9 Pro XL: connected-at-start, unplug, and replug all reported correctly, with the departing device still fully identified.
- **[cli]** No CLI changes; `0.7.2` just tracks the new library version.

## [0.26.0] - 2026-07-20

Library `0.26.0`, CLI `0.7.1`. Adding a field to `VirtualDeviceConfig` stops being a breaking change for downstream test suites.

### Changed

- **[lib] Adding a field to `VirtualDeviceConfig` no longer breaks your test setup.** The struct now implements `Default` (feature `virtual-device`), so you can build one as `VirtualDeviceConfig { storages: vec![...], ..Default::default() }` and set only what your test cares about. Every field addition until now was a compile break for anyone constructing the struct with an exhaustive literal (most recently `supports_partial_object_64` in 0.24.0); from here on a new field arrives with a working default instead. `storages` defaults to empty and still must be set: only you know which directory backs the storage, and `open_virtual` rejects an empty one with "VirtualDeviceConfig requires at least one storage". `VirtualStorageConfig` deliberately gets no `Default`, since a wrong `backing_dir` fails silently (an empty storage) rather than loudly.

## [0.25.0] - 2026-07-18

Library `0.25.0`, CLI `0.7.0`. Diagnostics so device-specific bugs can be captured by the reporter instead of reproduced on identical hardware (follow-up to the #18 Samsung cancel-wedge work in 0.24.0).

### Added

- **[lib] Opt-in `tracing` feature for device-protocol diagnostics.** Off by default (no dependency, no cost); enable it and install any `tracing` subscriber to emit events on the transaction and cancel/reset paths. Purpose-built so a bug reporter can capture what their device actually does (issue #18) instead of us reproducing on identical hardware. The cancel/reset path logs at `debug`, per-operation execution at `trace`.
- **[cli] `--trace` flag and richer `doctor`.** `mtp-rs --trace <cmd>` prints the cancel/reset diagnostics to stderr (`RUST_LOG` overrides for finer control, e.g. `mtp_rs=trace` for per-operation detail). `doctor` now prints device capabilities, and `doctor --probe-cancel` runs a cancel-health check (download a file, cancel mid-stream) that classifies the device as `healthy`, `wedged_recovered` (#18), or `errored` — the one-command artifact to attach to a freeze report.
- **[lib] `force_cancel_wedge(serial)` virtual-device test hook** (feature `virtual-device`). Arms a one-shot so the next `cancel_transfer` returns `Error::DeviceReset`, letting the #18 cancel-wedge contract be regression-tested with no hardware.

## [0.24.0] - 2026-07-18

Library `0.24.0`, CLI `0.6.0`.

### Fixed

- **[lib] Recover from the Samsung cancel wedge instead of hanging ([#18](https://github.com/vdavid/mtp-rs/issues/18)).** Cancelling a held-open streaming download wedged the PTP session on Samsung phones (reproduced on a Galaxy S23 Ultra): the drain idled out without ever seeing the closing Response container, the device stopped answering, and `cancel()` returned a false success so the consumer's next call hung, looking like a mid-transfer freeze. (This entry originally attributed the wedge to a large queued bulk backlog. Later hardware work showed transfer size isn't the trigger: a 36-byte file wedges it too. See the Unreleased entry.) `cancel_transfer` now detects the wedge (`GET_DEVICE_STATUS` timing out, distinct from an unsupported-op stall), issues a session-less USB `DEVICE_RESET` to un-stick the transport, and returns the new `Error::DeviceReset` instead of a false success. It deliberately does not reopen: post-reset these devices need a span of quiet to tear the old session down, so reopen is the caller's job (drop the device, wait a few seconds, reopen with idle-spaced backoff; hammering re-wedges it). `download_windowed` removes the need to cancel, though a dropped window future can still wedge the device through the recovery drain. Reported by [@qarmin](https://github.com/qarmin).
- **[lib] Ranged, windowed, and resumable downloads now work on devices that only advertise the 32-bit `GetPartialObject`.** Previously `download` with a `ByteRange::From`/`Range`, `download_windowed`, and `read_range` all hard-required `GetPartialObject64` (0x95C1), the 64-bit-offset op. Many PTP cameras (e.g. the Panasonic Lumix DMC-TZ61, [#12](https://github.com/vdavid/mtp-rs/issues/12)) only advertise the 32-bit `GetPartialObject` (0x101B), so these calls failed with `Unsupported`. The backend now falls back to the 32-bit op for any offset that fits in `u32` (files up to 4 GiB); a resume past 4 GiB still needs the 64-bit op (returns `Error::InvalidData`), and a device with neither op returns `Error::Unsupported`. The op selection (`plan_partial_read`) is unit-tested, and the 32-bit path is covered end-to-end against a virtual device configured without the 64-bit op.

### Changed

- **[lib] Breaking: new `PtpError::DeviceReset` variant.** The low-level `ptp::PtpError` enum is not `#[non_exhaustive]`, so code that exhaustively matches it must add an arm. The neutral `mtp::Error` gains the same `DeviceReset` variant but is `#[non_exhaustive]`, so matching it with a wildcard arm is unaffected. See the #18 fix above for what the variant means.
- **[workspace] Real-device debugging hub and test-harness hardening.** `docs/debugging.md` is now a debugging hub (macOS `ptpcamerad` blocker, software-reset recovery, fast-fail timeouts, Samsung/Android gotchas), linked from AGENTS.md. Integration tests read an `MTP_TEST_TIMEOUT_SECS` override (default 30) so a wedged or absent device skips fast instead of stalling, and an opened device reporting zero storages (a half-authorized phone) now skips cleanly instead of panicking.
- **[lib] Breaking (test support): `VirtualDeviceConfig` gains a `supports_partial_object_64: bool` field** (feature `virtual-device`). Set it `true` to keep the previous behavior; set it `false` to model a camera that only implements the 32-bit `GetPartialObject` and exercise the fallback above. Only affects code that constructs `VirtualDeviceConfig` directly (test setups).
- **[workspace] Opt-in integration-test env flags now test the value, not mere presence.** `MTP_RUN_SLOW_TESTS=0` and `MTP_RUN_DROP_RECOVERY=0` used to *enable* the gated test (a bare "is the var defined" check); they now correctly mean "off". Only `1`/`true`/`yes`/`on` enable. Reported by [@juleskers](https://github.com/juleskers) in [#12](https://github.com/vdavid/mtp-rs/issues/12).

## [0.23.0] - 2026-06-28

Library `0.23.0`, CLI `0.5.0`. The headline is **native Windows support** plus a **backend-neutral
`mtp::` API** that made it possible. This is a breaking release for library consumers.

### Added

- **[lib]** **Native Windows support via a Windows Portable Devices (WPD) COM backend.** On Windows the high-level `mtp::` API now auto-selects WPD, so phones work out of the box — no Zadig, no driver swap, no extra dependencies (mtp-rs uses Windows' own MTP stack). Covers listing, streaming download/upload, delete, rename, move, copy, thumbnails, capabilities, and device events. Pure Rust via the `windows` crate, `cfg(windows)`-gated. Hardware-verified on a Pixel 9 Pro XL. Resolves the Windows half of [#13](https://github.com/vdavid/mtp-rs/issues/13). ([#13](https://github.com/vdavid/mtp-rs/issues/13))
- **[lib]** `MtpDevice::capabilities()` returns a backend-neutral `Capabilities` (can_upload/delete/rename/move/copy/create_folder, supports_partial_download/thumbnails/events), replacing per-operation-code sniffing.
- **[lib]** `MtpDeviceBuilder::backend(Backend::{Auto, Usb, Wpd})` to override backend selection (e.g. force PTP-over-USB to a Zadig-bound camera on Windows).
- **[lib]** `Error::PermissionDenied` / `is_permission_denied()` (Linux udev / `EACCES`), distinct from `ExclusiveAccess`.

### Changed

- **[lib]** **`mtp::` is now backend-neutral (BREAKING).** `ObjectHandle`/`StorageId` are opaque `u64` session tokens; `ObjectInfo`/`DeviceInfo`/`StorageInfo`/`ObjectFormat`/`DateTime` and `mtp::Error` are neutral `mtp::` types (no longer leaked `ptp::` types). The rich low-level error is now `ptp::PtpError`; the `ptp::` API stays for camera/raw-PTP use (USB-only).
- **[lib]** **Downloads consolidated (BREAKING):** streaming `download(handle, ByteRange)` (whole-file/resume/slice), `download_to_vec`, and session-releasing `download_windowed`, plus `read_range` and `thumbnail`. Replaces the prior `download`/`download_partial`/`download_partial_64`/`download_stream`/`download_stream_from_offset` set.
- **[lib]** `MtpDevice::session()` removed; raw PTP access is via the `ptp::` module.
- **[lib]** `Storage::upload`/`upload_with_progress` accept borrowed (non-`'static`) streams and progress callbacks.
- **[cli]** Migrated to the neutral API and **now works on Windows** via the WPD backend. Same commands and output.
- **[cli]** Bump `mtp-rs` dependency to 0.23.0.
- **[workspace]** Internals reorganized around an `MtpBackend` seam (`UsbBackend` over PTP/USB, `WpdBackend` over WPD/COM), with a cross-backend conformance suite run against both the virtual device (CI) and a real device (Windows), plus a Windows CI job.

## [0.22.0] - 2026-06-27

### Added

- **[lib]** `Storage::download_windowed(handle, window_size)` reads a large file without monopolizing the PTP session: `next_window()` issues bounded `GetPartialObject64` transactions that each release the session, so a consumer can interleave other device work between windows. Drop it to stop early. ([fd3f9f6a](https://github.com/vdavid/mtp-rs/commit/fd3f9f6a))

### Changed

- **[workspace]** `test_drop_mid_stream_then_software_reconnect` is now opt-in behind `MTP_RUN_DROP_RECOVERY=1`: some camera firmware (Panasonic Lumix DMC-TZ61, [#12](https://github.com/vdavid/mtp-rs/issues/12)) wedges so hard on a mid-stream drop that only a USB replug recovers it, poisoning the rest of the suite. ([760b910a](https://github.com/vdavid/mtp-rs/commit/760b910a))
- **[cli]** Bump `mtp-rs` dependency to 0.22.0. No functional CLI change; tracks the library release. ([12a3ec0d](https://github.com/vdavid/mtp-rs/commit/12a3ec0d))

## [0.21.0] - 2026-06-22

Library `0.21.0`, CLI `0.4.1`. Adds resumable streaming downloads (lib). The CLI is a dependency-bump rebuild against the new lib, with no CLI-facing change.

### Added

- **[lib]** `Storage::download_stream_from_offset(handle, offset)`: resumable streaming downloads. Streams `[offset, size)` to EOF via `GetPartialObject64` (64-bit offset, so files over 4 GB resume). A consumer can `cancel()` to free the session, then reopen from the kept byte count. ([4182d34d](https://github.com/vdavid/mtp-rs/commit/4182d34d))

### Changed

- **[workspace]** `test_drop_mid_stream_then_software_reconnect` attempts recovery with `reset_device()`. It poisons the session (drops a download without cancel/drain); on PTP cameras a plain reopen can't clear the stuck transaction, so the test follows the failed reopen with a transport-level `reset_device()`. ([8b356cf0](https://github.com/vdavid/mtp-rs/commit/8b356cf0))

## [0.20.0] - 2026-06-19

Library `0.20.0`, CLI `0.4.0`. First substantive release since `0.18.0` (`0.19.0` was an inadvertent no-op re-release, see below).

### Added

- **[lib]** `MtpDevice::supports_upload()` and `DeviceInfo::supports_upload()`. True when the device advertises both `SendObjectInfo` and `SendObject`, so consumers can skip write attempts on read-only PTP cameras. Means "worth attempting", not "guaranteed" (Fuji advertises write yet rejects per-operation). ([2bc64bfd](https://github.com/vdavid/mtp-rs/commit/2bc64bfd))
- **[lib]** 13 new `OperationCode` variants covering the rest of the standard PTP set and the common MTP object-property extensions (`GetObjectPropList`, etc.). These previously decoded as `Unknown(...)`, muddying diagnostics. Technically breaking for exhaustive matches on `OperationCode`. ([82de0c8b](https://github.com/vdavid/mtp-rs/commit/82de0c8b))
- **[lib]** Device reset: `Transport::reset_device()` and `PtpDevice::reset_device()`. Sends the USB SIC Device Reset, clears halted bulk endpoints, and drains stale data. Recovers a device stuck after an interrupted transfer, even without a session. From [#12](https://github.com/vdavid/mtp-rs/issues/12). ([2b68d55e](https://github.com/vdavid/mtp-rs/commit/2b68d55e))
- **[cli]** New `mtp-rs reset` command exposing the device reset, with `--device`/`--location`/`--json` and a post-reset `GetDeviceInfo` verification. Documented in `docs/cli.md`. ([2b68d55e](https://github.com/vdavid/mtp-rs/commit/2b68d55e))
- **[lib]** `rekey_virtual_object(serial, rel_path)`: reassigns a tracked object's handle in place, so the old handle fails and a fresh parent listing surfaces the new one. Reproduces Android MediaProvider's handle re-keying, so consumers can drive stale-handle recovery (feature `virtual-device`). ([70b9f2a9](https://github.com/vdavid/mtp-rs/commit/70b9f2a9))

### Fixed

- **[lib]** Sessions self-heal after a transaction is abandoned mid-flight. If an operation's future is dropped after its command goes out but before its response is drained, the transaction-ID stream desyncs forever. `PtpSession` now drains the pipe before the next operation. No API change. ([1cc4ad2c](https://github.com/vdavid/mtp-rs/commit/1cc4ad2c))
- **[lib]** Unparseable datetimes in `ObjectInfo` no longer fail the whole listing. The Lumix DMC-TZ61 ([#12](https://github.com/vdavid/mtp-rs/issues/12)) reports `20480000T000000` as a "no date" sentinel; parsing is now lenient (becomes `None`), packing stays strict. ([82de0c8b](https://github.com/vdavid/mtp-rs/commit/82de0c8b))
- **[lib]** Devices stay usable after a mid-transfer cancel. `cancel_transfer` now polls SIC GET_DEVICE_STATUS until the device clears Device_Busy; without it the Lumix DMC-TZ61 ([#12](https://github.com/vdavid/mtp-rs/issues/12)) timed out after a "successful" cancel. ([6a90769b](https://github.com/vdavid/mtp-rs/commit/6a90769b))
- **[lib]** Endpoint halts are cleared after STALL. Cameras stall a bulk endpoint to signal unsupported operations, and the halt persists across process restarts, wedging the next run at `GetDeviceInfo`. Every bulk/interrupt completion site now clears the halt before surfacing the error. ([6a90769b](https://github.com/vdavid/mtp-rs/commit/6a90769b))
- **[lib]** PTP strings truncate at the first NUL. `unpack_string` stripped exactly one trailing NUL, but the Lumix pads its serial to a fixed width with multiple NULs, so one leaked into the decoded `String`. Per spec, anything from the first NUL on is padding. ([5a127047](https://github.com/vdavid/mtp-rs/commit/5a127047))

### Changed

- **[workspace]** Destructive integration tests now skip cleanly on read-only devices. They check `supports_upload()` before writing, and the harness logs the specific skip reason instead of a generic setup-failed message. Triggered by the Lumix DMC-TZ61 report in [#12](https://github.com/vdavid/mtp-rs/issues/12). ([2bc64bfd](https://github.com/vdavid/mtp-rs/commit/2bc64bfd))
- **[workspace]** Download tests find their test file much faster. The recursive fallback is now a breadth-first streaming search that stops at the first size match (seconds, not 10+ minutes on PTP cameras), the find is cached across tests, and `MTP_TEST_READFILE` pins an exact path. ([5a127047](https://github.com/vdavid/mtp-rs/commit/5a127047))
- **[workspace]** The `diagnose` example bounds its recursive listing to 200 objects. The unbounded version ran 10+ minutes on cameras with slow metadata fetches, tempting mid-traversal Ctrl+C, which wedges some devices. ([4872c21b](https://github.com/vdavid/mtp-rs/commit/4872c21b))

## [0.19.0] - 2026-05-30

Inadvertent no-op re-release: the source is byte-identical to 0.18.0 (only `Cargo.toml` / `Cargo.lock` / this changelog differ). Published while chasing a downstream (Cmdr) build failure that looked like a missing `UploadError`, but the real cause was a transient stale lockfile. Adds nothing; prefer 0.18.0 / 0.2.0. ([6a917b2b](https://github.com/vdavid/mtp-rs/commit/6a917b2b))

## [0.18.0] - 2026-05-30

### Changed

- **[lib]** Breaking: `Storage::upload` and `upload_with_progress` now return `Result<ObjectHandle, UploadError>`. On data-phase failure the device holds a partial; `UploadError` carries `source: Error` plus `partial: Option<ObjectHandle>`. The library doesn't auto-delete; the consumer owns cleanup-or-resume. ([b36c3849](https://github.com/vdavid/mtp-rs/commit/b36c3849))
- **[lib]** Virtual device now creates the object at `SendObjectInfo` time, matching real devices, so a cancelled upload leaves a real, deletable object at the `partial` handle. `SendObject` then overwrites it and emits `ObjectInfoChanged`, preserving the one-`ObjectAdded`-per-upload dedup contract. ([b36c3849](https://github.com/vdavid/mtp-rs/commit/b36c3849))
- **[cli]** `put` now cleans up the partial object on upload failure (best-effort; no resume story), then reports the underlying error. Bumped to 0.2.0 for the breaking lib dependency. ([b36c3849](https://github.com/vdavid/mtp-rs/commit/b36c3849))

## [0.17.0] - 2026-05-27

### Added

- **[cli]** New `mtp-rs-cli` crate (initial release 0.1.0): a universal MTP file transfer CLI (binary `mtp-rs`). 11 subcommands (`ls`, `put`, `get`, `rm`, `doctor`, ...), all with `--json`, stable exit codes, and streaming progress. By @dtretyakov in [#11](https://github.com/vdavid/mtp-rs/pull/11). ([8e6adc0a](https://github.com/vdavid/mtp-rs/commit/8e6adc0a))
- **[lib]** Match-reason on enumerated devices. `MtpDeviceInfo::match_reason` and `UsbDeviceInfo::match_reason` carry a new four-variant `MtpMatchReason` enum explaining why a USB device was classified as MTP. Both info structs are `#[non_exhaustive]`, so this is additive. ([60b69e0d](https://github.com/vdavid/mtp-rs/commit/60b69e0d))
- **[lib]** Garmin-style `MTP` interface-string detection. Devices that expose MTP on a vendor-class (`0xff/0xff`) interface but advertise an `interface_string` of `MTP` are now classified correctly. Verified on Garmin Venu 2/2S. ([60b69e0d](https://github.com/vdavid/mtp-rs/commit/60b69e0d))

### Changed

- **[workspace]** Repo is now a Cargo workspace: library at `crates/mtp-rs/`, CLI at `crates/mtp-rs-cli/`. The public lib API is unchanged, so `mtp-rs = "0.17"` rebuilds without code changes. The split keeps the library free of CLI-only deps (`clap`, `serde`, `tokio`). ([4882d0d7](https://github.com/vdavid/mtp-rs/commit/4882d0d7))
- **[lib]** Virtual-device watcher restored to `RecommendedWatcher`. The CLI PR temporarily swapped to `PollWatcher`, which made every virtual-device user scan their backing dirs 20×/sec forever. Restored to the kernel-driven native watcher. ([2ab20378](https://github.com/vdavid/mtp-rs/commit/2ab20378))

### Notes

- The `mtp-rs` binary moved from the library crate's `cli` feature to the new `mtp-rs-cli` crate. The `cli` feature on the lib is gone. Installation is now `cargo install mtp-rs-cli`; the binary name is still `mtp-rs`.
- Library MSRV stays at 1.85. The CLI crate also targets MSRV 1.85.

## [0.16.0] - 2026-05-23

### Added

- **Event-driven backing-dir drain for virtual devices.** Replaces the old ≥600 ms sleep with actual quiescence after a virtual device's backing dir is recreated externally:
  - `dropped_paths_since_pause(serial) -> Vec<PathBuf>`: paths the watcher dropped while paused, oldest first (the primary primitive).
  - `was_path_dropped(serial, suffix) -> bool`: convenience wrapper for the sentinel-file pattern.
  - `clear_dropped_paths(serial)`: empties the ring after a drain.
  - The ring is capped at `DROPPED_PATHS_CAP = 1024` (public constant); oldest evicted past the cap.
  ([5da20642](https://github.com/vdavid/mtp-rs/commit/5da20642))
- **Refcounted pause/resume.** `pause_watcher` now increments a `pause_count` instead of flipping a `bool`; `WatcherGuard::drop` decrements it, so the watcher resumes only when the last guard drops and concurrent test drains compose instead of racing. ([5da20642](https://github.com/vdavid/mtp-rs/commit/5da20642))

### Changed

- `WatcherGuard::drop` no longer unconditionally clears the paused flag; it decrements the refcount and resumes only at zero. Single-guard usage is unchanged; multi-guard now composes instead of racing. ([5da20642](https://github.com/vdavid/mtp-rs/commit/5da20642))

### Notes

- Behind the existing `virtual-device` feature; production consumers without it compile zero of this.
- The watcher integration is exercised end-to-end by downstream E2E suites (Cmdr's MTP Playwright lane uses the sentinel-file pattern); the library's unit tests cover the observation API, refcount composition, and ring eviction.

## [0.15.0] - 2026-05-19

### Added

- **Cooperative cancellation for long list and delete operations via `CancelToken`** (re-exported at the crate root). New `_with_cancel` variants take an `Option<&CancelToken>`; when it flips, iteration returns `Err(Error::Cancelled)` at the next per-object boundary. ([31a66c70](https://github.com/vdavid/mtp-rs/commit/31a66c70))
- **`CancelToken::from_arc(Arc<AtomicBool>)`** wraps a consumer-owned atomic so existing cancellation state flips the token directly. No second polling task, no two-way sync. ([31a66c70](https://github.com/vdavid/mtp-rs/commit/31a66c70))
- The existing `list_objects` / `list_objects_stream` / `delete` entry points stay for backwards compatibility; they delegate to the `_with_cancel` variants with `None`. ([31a66c70](https://github.com/vdavid/mtp-rs/commit/31a66c70))

### Notes

- Streaming downloads keep their existing USB SIC class-cancel path via `FileDownload::cancel`. That handles a different problem (one long bulk-IN to drain) and stays unchanged.
- Per-handle cancellation only fires at per-object boundaries, which is where slow listings spend their time. Mid-transaction cancel would be more complex and less safe.

## [0.14.0] - 2026-05-15

### Added

- **Negotiated USB link speed on enumerated devices.** `MtpDeviceInfo::speed` and `UsbDeviceInfo::speed` now carry `Option<UsbSpeed>` (re-exported), so consumers can surface negotiated speed without a direct `nusb` dependency. The value is the slowest of host port, cable, and device. ([f55638cb](https://github.com/vdavid/mtp-rs/commit/f55638cb))

### Changed

- **Breaking**: `MtpDeviceInfo` and `UsbDeviceInfo` gained a `speed: Option<UsbSpeed>` field and are now `#[non_exhaustive]` (future field additions are non-breaking). Consumers that built either via struct literal now need `..` or named construction. ([f55638cb](https://github.com/vdavid/mtp-rs/commit/f55638cb))

## [0.13.3] - 2026-05-05

### Fixed

- **`PtpDevice::get_device_info()` now handles a container header and payload split across separate USB transfers.** Some devices (Garmin Forerunner 955) split them; the session-less path bailed with a length mismatch. Reported on [#10](https://github.com/vdavid/mtp-rs/pull/10). ([77a7d3e7](https://github.com/vdavid/mtp-rs/commit/77a7d3e7))

### Changed

- `PtpDevice::transport` is now `Arc<dyn Transport>` instead of `Arc<NusbTransport>`. Internal change, no public API impact. Enables mock-based unit testing of session-less paths. ([77a7d3e7](https://github.com/vdavid/mtp-rs/commit/77a7d3e7))
- AGENTS.md now codifies the multi-transfer receive convention, so future code paths that parse a `DataContainer` know to accumulate USB transfers until the full container is in hand. ([e16199c0](https://github.com/vdavid/mtp-rs/commit/e16199c0))

## [0.13.2] - 2026-04-27

### Fixed

- **Root listing is now fast on Kindle and similar devices.** `list_objects_stream(None)` took the slow `parent=0` path on non-`android.com` devices; the fast `parent=0xFFFFFFFF` path is now tried first, `parent=0` as fallback. [#9](https://github.com/vdavid/mtp-rs/pull/9), closes [#8](https://github.com/vdavid/mtp-rs/issues/8). ([688faa30](https://github.com/vdavid/mtp-rs/commit/688faa30))

### Changed

- The `is_android()` gate inside `list_objects_stream` is gone; the unified fast-path/fallback handles Android, Kindle, Samsung, and Fuji without vendor detection. The `is_android()` check inside `list_objects_recursive_auto` remains (it gates a different workaround). ([688faa30](https://github.com/vdavid/mtp-rs/commit/688faa30))

## [0.13.1] - 2026-04-17

### Fixed

- **`get_object_info()` and `list_objects()` now return the real u64 size for files larger than 4 GB.** The standard `ObjectInfo` dataset saturates size at `u32::MAX`; the new logic auto-resolves the full size via `GetObjectPropValue(ObjectSize)` on saturation, falling back where unsupported. ([5522040a](https://github.com/vdavid/mtp-rs/commit/5522040a))

### Added

- **`PtpSession::get_object_info_full()`**: low-level method that fetches ObjectInfo and resolves the u64 size when saturated. ([5522040a](https://github.com/vdavid/mtp-rs/commit/5522040a))
- 5 new unit tests covering saturation detection, fallback, and the `u32::MAX`-exact edge case. ([5522040a](https://github.com/vdavid/mtp-rs/commit/5522040a))
- Virtual-device integration test that creates a 5 GB sparse file and verifies size resolution end-to-end. ([5522040a](https://github.com/vdavid/mtp-rs/commit/5522040a))

### Changed

- Doc comment on `ObjectInfo::size` updated to reflect the new auto-resolution behavior. ([5522040a](https://github.com/vdavid/mtp-rs/commit/5522040a))

## [0.13.0] - 2026-04-17

### Added

- **`Storage::download_partial_64()`** and **`PtpSession::get_partial_object_64()`**: byte-range reads with 64-bit offsets via the Android/MTP `GetPartialObject64` extension (0x95C1), enabling partial reads beyond the 4 GB boundary. Tested end-to-end on a Pixel 9 Pro XL with an 8 GB file. ([dd979a3d](https://github.com/vdavid/mtp-rs/commit/dd979a3d))
- **`OperationCode::GetPartialObject64`** variant. ([dd979a3d](https://github.com/vdavid/mtp-rs/commit/dd979a3d))
- Virtual device supports `GetPartialObject64` and advertises it in `operations_supported`. ([dd979a3d](https://github.com/vdavid/mtp-rs/commit/dd979a3d))
- New example `test_partial_download_64.rs` for real-device verification. ([dd979a3d](https://github.com/vdavid/mtp-rs/commit/dd979a3d))
- 3 new unit tests covering byte-range reads and 64-bit offset correctness. ([dd979a3d](https://github.com/vdavid/mtp-rs/commit/dd979a3d))

### Changed

- Documented the 4 GB offset limitation on `download_partial()` / `get_partial_object()` and cross-linked to the new 64-bit variants. ([dd979a3d](https://github.com/vdavid/mtp-rs/commit/dd979a3d))

## [0.12.0] - 2026-04-16

### Added

- **`Transport::send_bulk_streaming()`**: sends data as a continuous USB transfer from a stream of chunks, with proper ZLP termination. The default impl buffers and calls `send_bulk()`; `NusbTransport` streams in 256KB USB transfers via nusb's low-level endpoint API. ([cc9035ec](https://github.com/vdavid/mtp-rs/commit/cc9035ec))

### Changed

- **Breaking:** `Storage::upload()` and `upload_with_progress()` now require `Send` on the stream type parameter. ([cc9035ec](https://github.com/vdavid/mtp-rs/commit/cc9035ec))
- **Breaking:** `Transport` has a new `send_bulk_streaming()` method (with a default impl, so most custom impls need no change). ([cc9035ec](https://github.com/vdavid/mtp-rs/commit/cc9035ec))
- **Breaking:** `PtpSession::execute_with_send_stream()` and `send_object_stream()` now require `Send` on the stream type parameter. ([cc9035ec](https://github.com/vdavid/mtp-rs/commit/cc9035ec))
- Uploads stream data directly to USB instead of buffering the entire file. Peak memory during upload drops from O(file_size) to O(256KB). ([cc9035ec](https://github.com/vdavid/mtp-rs/commit/cc9035ec))

## [0.11.1] - 2026-04-15

### Changed

- **Streaming uploads:** `Storage::upload()` and `upload_with_progress()` now stream data directly to USB via `send_object_stream` instead of buffering the entire file. Peak memory during upload drops from O(file_size) to O(chunk_size). The API is unchanged. ([cee3e683](https://github.com/vdavid/mtp-rs/commit/cee3e683))

## [0.11.0] - 2026-04-10

### Added

- **Safe mid-stream download cancellation:** `FileDownload::cancel(idle_timeout)` and `ReceiveStream::cancel(idle_timeout)` safely abort in-progress downloads via the USB Still Image Class cancel mechanism, leaving the session healthy for subsequent operations. ([e87e12a6](https://github.com/vdavid/mtp-rs/commit/e87e12a6))
- **`Transport::cancel_transfer()`** trait method with implementations for `NusbTransport`, `MockTransport`, and `VirtualTransport`. ([e87e12a6](https://github.com/vdavid/mtp-rs/commit/e87e12a6))
- **`DEFAULT_CANCEL_TIMEOUT`** (300ms) constant for the recommended cancel drain timeout. ([e87e12a6](https://github.com/vdavid/mtp-rs/commit/e87e12a6))
- **`EventCode::CancelTransaction`** variant (0x4001) in the event code enum. ([e87e12a6](https://github.com/vdavid/mtp-rs/commit/e87e12a6))
- **`EventContainer::to_bytes()`** serialization method (completes the `from_bytes`/`to_bytes` pair). ([e87e12a6](https://github.com/vdavid/mtp-rs/commit/e87e12a6))
- `#[must_use]` on `ReceiveStream` and `FileDownload`: the compiler warns if dropped without consuming or cancelling. ([e87e12a6](https://github.com/vdavid/mtp-rs/commit/e87e12a6))
- `debug_assert` in `ReceiveStream::Drop` catches accidental mid-stream drops during development. ([e87e12a6](https://github.com/vdavid/mtp-rs/commit/e87e12a6))

### Fixed

- `collect_with_progress` now properly cancels the USB transfer when the progress callback returns `ControlFlow::Break`, instead of just dropping the stream (which corrupted the session). ([e87e12a6](https://github.com/vdavid/mtp-rs/commit/e87e12a6))

### Changed

- **Breaking:** `Transport` now requires `cancel_transfer()`; custom implementations must add this method. ([e87e12a6](https://github.com/vdavid/mtp-rs/commit/e87e12a6))
- `NusbTransport` now stores the USB `Interface` and interface number (needed for SIC cancel control transfers). ([e87e12a6](https://github.com/vdavid/mtp-rs/commit/e87e12a6))

## [0.10.0] - 2026-04-09

### Added

- **Public low-level PTP execution primitives:** `PtpSession::execute()`, `execute_with_receive()`, and `execute_with_send()` are now public, enabling vendor-specific and non-standard MTP operations without forking the crate. ([8c07072a](https://github.com/vdavid/mtp-rs/commit/8c07072a))
- **`MtpDevice::session()`** accessor to reach the underlying `PtpSession` from the high-level API. ([8c07072a](https://github.com/vdavid/mtp-rs/commit/8c07072a))
- **Split header/data send mode:** `set_split_header_data()` / `is_split_header_data()` for devices that need the 12-byte PTP container header and payload as separate USB bulk transfers (also in streaming sends). ([8c07072a](https://github.com/vdavid/mtp-rs/commit/8c07072a))
- **Custom VID/PID device discovery:** `MtpDevice::list_devices_with_known()` and `MtpDeviceBuilder::known_devices()` include devices with non-standard USB descriptors in enumeration and open. ([8c07072a](https://github.com/vdavid/mtp-rs/commit/8c07072a))
- **`MtpDeviceBuilder::open_nusb_device()`** escape hatch for consumers doing their own USB enumeration or hotplug watching. ([8c07072a](https://github.com/vdavid/mtp-rs/commit/8c07072a))
- **Permissive interface scan on open:** two-pass scan (strict MTP class first, then endpoint-layout fallback) for devices with non-standard interface descriptors. ([8c07072a](https://github.com/vdavid/mtp-rs/commit/8c07072a))
- **macOS `SetConfiguration(1)` retry:** automatically recovers when IOKit doesn't publish interface services for vendor-class devices. ([8c07072a](https://github.com/vdavid/mtp-rs/commit/8c07072a))

### Fixed

- Gate the macOS-only `is_interface_unpublished` helper with `#[cfg(target_os = "macos")]` to fix a dead-code warning on non-macOS builds. ([d1d32798](https://github.com/vdavid/mtp-rs/commit/d1d32798))

Thanks to [@kelchm](https://github.com/kelchm) for contributing the low-level primitives ([#4](https://github.com/vdavid/mtp-rs/pull/4)).

## [0.9.1] - 2026-04-08

### Fixed

- Virtual device's `handle_move_object` now emits MTP events (`ObjectInfoChanged` + `StorageInfoChanged`), fixing a bug where consumers' event loops had no signal to refresh directory listings after a move. ([fc5bfd7f](https://github.com/vdavid/mtp-rs/commit/fc5bfd7f))

## [0.9.0] - 2026-04-08

### Added

- `pause_watcher(serial)` returns an RAII `WatcherGuard` that suppresses filesystem events while alive, preventing a race where stale OS deletion events corrupt the object tree after a rescan. ([eacd8bcf](https://github.com/vdavid/mtp-rs/commit/eacd8bcf))
- `WatcherGuard` re-exported from crate root. ([eacd8bcf](https://github.com/vdavid/mtp-rs/commit/eacd8bcf))

## [0.8.0] - 2026-04-07

### Added

- `rescan_virtual_device(serial)` force-syncs the virtual device's in-memory object tree with the filesystem, removing stale entries and adding new ones with proper MTP event queuing. ([61df6d26](https://github.com/vdavid/mtp-rs/commit/61df6d26))
- Active-state registry for live `VirtualTransport` instances, with `Drop`-based cleanup. ([61df6d26](https://github.com/vdavid/mtp-rs/commit/61df6d26))
- `RescanSummary` type re-exported from crate root. ([61df6d26](https://github.com/vdavid/mtp-rs/commit/61df6d26))

## [0.7.2] - 2026-04-03

### Fixed

- Fix fs watcher dedup on macOS: skip the FSEvents startup event for the backing directory itself (empty relative path) that produced a spurious `ObjectAdded`. ([96e22ac2](https://github.com/vdavid/mtp-rs/commit/96e22ac2))
- Bump `actions/checkout` from v4 to v5 in CI (Node.js 20 deprecation). ([96e22ac2](https://github.com/vdavid/mtp-rs/commit/96e22ac2))

## [0.7.0] - 2026-04-03

### Added

- `MtpDevice` now implements `Clone` (cheap, wraps `Arc` internally), enabling consumers to clone the device for concurrent event polling. ([1cc56eb0](https://github.com/vdavid/mtp-rs/commit/1cc56eb0))

### Fixed

- Fix fs watcher dedup on macOS: event processing moved from the watcher callback (FSEvents thread) to `receive_interrupt` (caller thread), eliminating cross-thread timing issues. ([a875fb8e](https://github.com/vdavid/mtp-rs/commit/a875fb8e))
- Fix incorrect `progress.percent().unwrap_or(0.0)` in `FileDownload::collect_with_progress` doc example (`percent()` returns `f64`, not `Option`). ([c9d39a84](https://github.com/vdavid/mtp-rs/commit/c9d39a84))

### Changed

- 13 doc examples converted from `ignore` to `no_run` with hidden boilerplate (now compile-checked, catches API drift). ([c9d39a84](https://github.com/vdavid/mtp-rs/commit/c9d39a84))

## [0.6.1] - 2026-04-03

### Fixed

- Fix flaky `fs_watcher_dedup` test on macOS: assert on `ObjectAdded` count instead of total event count, since extra `StorageInfoChanged` events may be generated. ([287cbd54](https://github.com/vdavid/mtp-rs/commit/287cbd54))

## [0.6.0] - 2026-04-02

### Added

- Filesystem watcher for virtual devices: when `watch_backing_dirs` is `true`, the virtual device detects files created or removed directly in backing directories (bypassing MTP) and emits `ObjectAdded`/`ObjectRemoved` events, matching real device behavior. ([a098db58](https://github.com/vdavid/mtp-rs/commit/a098db58))
- `VirtualDeviceConfig::watch_backing_dirs` field to opt in/out of filesystem watching. ([a098db58](https://github.com/vdavid/mtp-rs/commit/a098db58))
- `notify` v8 dependency (optional, gated behind the `virtual-device` feature). ([a098db58](https://github.com/vdavid/mtp-rs/commit/a098db58))

### Changed

- **Breaking:** MSRV raised from 1.79 to 1.85. ([9a74acea](https://github.com/vdavid/mtp-rs/commit/9a74acea))
- Upgraded `notify` from v7 to v8 (drops the unmaintained `instant` transitive dep). ([9a74acea](https://github.com/vdavid/mtp-rs/commit/9a74acea))
- Upgraded `thiserror` from v1 to v2 (faster proc-macro compilation, no API changes). ([9a74acea](https://github.com/vdavid/mtp-rs/commit/9a74acea))
- Unpinned `proptest` dev-dependency (was pinned to `=1.5.0` for MSRV 1.79). ([9a74acea](https://github.com/vdavid/mtp-rs/commit/9a74acea))

## [0.5.1] - 2026-04-01

### Fixed

- Fix clippy `needless_borrow` warnings on Rust 1.79 (MSRV) in the virtual device module. ([97e84b1f](https://github.com/vdavid/mtp-rs/commit/97e84b1f))

## [0.5.0] - 2026-04-01

### Added

- `virtual-device` feature for testing MTP client code without USB hardware:
  - `VirtualTransport` implements the `Transport` trait against local filesystem directories, speaking the full MTP/PTP binary protocol so `MtpDevice`, `Storage`, and `PtpSession` work unchanged.
  - `MtpDevice::builder().open_virtual(config)` creates a virtual device directly.
  - `register_virtual_device()` / `unregister_virtual_device()` integrate with `list_devices()`, `open_by_location()`, and `open_by_serial()`.
  - Supports 16 MTP operations, path-traversal protection on writes, configurable `event_poll_interval`, read-only storage, and zero changes to existing code paths when the feature is disabled.
  ([c5fde2c1](https://github.com/vdavid/mtp-rs/commit/c5fde2c1))

## [0.4.2] - 2026-04-01

### Fixed

- Send `OpenSession` with `transaction_id=0` (session-less) per PTP spec, fixing Kindle and other strict PTP devices rejecting the session ([#2](https://github.com/vdavid/mtp-rs/pull/2), thanks [@num13ru](https://github.com/num13ru)). ([e6c67902](https://github.com/vdavid/mtp-rs/commit/e6c67902))
- Fix stale `next_event()` docs after timeout removal. ([6f679991](https://github.com/vdavid/mtp-rs/commit/6f679991))
- Fix README indentation broken by PR #2. ([8e665276](https://github.com/vdavid/mtp-rs/commit/8e665276))

## [0.4.1] - 2026-03-24

### Fixed

- Detect vendor-specific MTP devices (e.g. Amazon Kindle) that use USB class 0xFF with non-standard subclass/protocol ([#1](https://github.com/vdavid/mtp-rs/issues/1)). ([d565bcf4](https://github.com/vdavid/mtp-rs/commit/d565bcf4))

## [0.4.0] - 2026-03-20

### Changed

- Replaced platform-specific IOKit/location_id code with nusb's cross-platform `port_chain()` + `bus_id()`. ([e126e4ae](https://github.com/vdavid/mtp-rs/commit/e126e4ae))
- **Breaking:** `location_id` values will differ from previous versions (now derived from USB topology instead of macOS IOKit). ([e126e4ae](https://github.com/vdavid/mtp-rs/commit/e126e4ae))
- Fixed a timeout race condition: `receive_bulk` now leaves USB transfers pending on timeout instead of cancelling them, preventing data loss on retry. ([a9a0da90](https://github.com/vdavid/mtp-rs/commit/a9a0da90))
- `receive_interrupt()` now awaits indefinitely for events (no timeout); callers should use async cancellation. ([31e014ef](https://github.com/vdavid/mtp-rs/commit/31e014ef))
- Switched from `std::sync::Mutex` to `futures::lock::Mutex` for async-safe locking across `.await` points. ([a9a0da90](https://github.com/vdavid/mtp-rs/commit/a9a0da90))
- Re-added the `futures-timer` dependency for async timeout support. ([a9a0da90](https://github.com/vdavid/mtp-rs/commit/a9a0da90))

### Removed

- Removed the `io-kit-sys` and `core-foundation` macOS dependencies (location info now provided by nusb). ([e126e4ae](https://github.com/vdavid/mtp-rs/commit/e126e4ae))
- **Breaking:** Removed `event_timeout`, `DEFAULT_EVENT_TIMEOUT`, `set_event_timeout()`, `event_timeout()`, and `open_with_timeouts()` from `NusbTransport`. ([31e014ef](https://github.com/vdavid/mtp-rs/commit/31e014ef))
- **Breaking:** Removed `event_timeout()` from `MtpDeviceBuilder`. ([31e014ef](https://github.com/vdavid/mtp-rs/commit/31e014ef))

## [0.3.0] - 2026-03-20

### Removed

- Removed the `futures-timer` dependency (timeouts now handled by nusb internally). ([e41a2952](https://github.com/vdavid/mtp-rs/commit/e41a2952))

### Changed

- **Breaking:** Upgraded `nusb` dependency from 0.1 to 0.2. ([e41a2952](https://github.com/vdavid/mtp-rs/commit/e41a2952))
- **Breaking:** MSRV raised from 1.75 to 1.79. ([e41a2952](https://github.com/vdavid/mtp-rs/commit/e41a2952))
- **Breaking:** `UsbDeviceInfo::open()` now returns `Result<nusb::Device, nusb::Error>` instead of `Result<nusb::Device, std::io::Error>`. ([e41a2952](https://github.com/vdavid/mtp-rs/commit/e41a2952))
- **Breaking:** Removed `NusbTransport::bulk_in_endpoint()`, `bulk_out_endpoint()`, and `interrupt_in_endpoint()` accessors. ([e41a2952](https://github.com/vdavid/mtp-rs/commit/e41a2952))
- Improved MTP device detection: can now detect composite MTP devices without opening them (nusb 0.2 exposes interface info on `DeviceInfo`). ([e41a2952](https://github.com/vdavid/mtp-rs/commit/e41a2952))
- Transport internals now use nusb 0.2's `Endpoint` pattern with `transfer_blocking` instead of single-shot methods. ([e41a2952](https://github.com/vdavid/mtp-rs/commit/e41a2952))

## [0.2.0] - 2026-03-17

### Added

- `Storage::list_objects_stream()`: streaming object listing that yields `ObjectInfo` items one at a time from USB, with `total()` and `fetched()` for progress reporting. ([69109022](https://github.com/vdavid/mtp-rs/commit/69109022))
- `ObjectListing` struct for iterating over streamed results. ([69109022](https://github.com/vdavid/mtp-rs/commit/69109022))
- Reproducible benchmark suite (`mtp-bench` crate at `benchmarks/mtp-rs-vs-libmtp/`) comparing mtp-rs against libmtp. ([f11bb514](https://github.com/vdavid/mtp-rs/commit/f11bb514))
- Benchmark results in README: mtp-rs is 1.06x–4.04x faster across all operations. ([9b7b407b](https://github.com/vdavid/mtp-rs/commit/9b7b407b))
- Release process documentation (`docs/releasing.md`). ([79f09a1f](https://github.com/vdavid/mtp-rs/commit/79f09a1f))

### Changed

- `list_objects()` refactored to use `list_objects_stream()` internally, with no behavior change. ([69109022](https://github.com/vdavid/mtp-rs/commit/69109022))

## [0.1.0] - 2026-02-20

Initial release targeting modern Android devices.

### Added

- Connect to Android phones/tablets over USB
- List, download, upload, delete, move, and copy files
- Create and delete folders
- Stream large file downloads with progress tracking
- Listen for device events (file added, storage removed, etc.)
- Two-layer API: high-level `mtp::` and low-level `ptp::`
- Runtime-agnostic async design (works with tokio, async-std, etc.)
- Pure Rust implementation using `nusb` for USB access
- Smart recursive listing that auto-detects Android and uses manual traversal
- `Storage::list_objects_recursive_manual()` for explicit manual traversal
- `Storage::list_objects_recursive_native()` for explicit native MTP recursive listing
- Android device detection via the `"android.com"` vendor extension
- Integration tests organized into `readonly` and `destructive` categories
- Serial test execution to avoid USB device conflicts
- Diagnostic example (`examples/diagnose.rs`)

### Fixed

- MTP device detection for composite USB devices (class 0): most Android phones expose MTP as one interface, so we now inspect interface descriptors to find it
- Large MTP data containers (>64KB): data spanning multiple USB transfers is reassembled before parsing
- Recursive listing on Android: Android ignores `ObjectHandle::ALL`, so we detect this and use manual traversal
- Integration tests use the `Download/` folder instead of root (Android doesn't allow creating files/folders in storage root)

### Changed

- `list_objects_recursive()` now automatically chooses the best strategy: manual folder-by-folder traversal on Android, native recursive (with fallback to manual) elsewhere

### Not included (by design)

- MTPZ (DRM extension for old devices)
- Playlist and metadata syncing
- Vendor-specific extensions
- Legacy device quirks database
