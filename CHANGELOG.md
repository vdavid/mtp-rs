# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This file covers both published crates in the workspace:

- `mtp-rs` (the library)
- `mtp-rs-cli` (the CLI binary, new in this release)

Entries are grouped by release. Each entry tags which crate it applies to with **[lib]**, **[cli]**, or **[workspace]** for repo-wide changes.

## [Unreleased]

### Fixed

- **[lib] Root-level creates retry with the selected storage ID only when parent `0` receives `InvalidParentObject` or `InvalidObjectHandle` for that exact `SendObjectInfo` transaction.** The standard parent remains the first attempt, object data cannot start before the retry succeeds, nested writes are unchanged, and ambiguous transport/session failures are never retried.

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
