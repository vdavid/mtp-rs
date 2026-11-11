# mtp-rs

Pure-Rust MTP/PTP library with no C dependencies. Two-layer API: `mtp::` for high-level file transfer, `ptp::` for low-level protocol access (cameras). Zero FFI - no libmtp, no libusb, just async Rust on `nusb`.

This repo is a Cargo workspace. The library lives in `crates/mtp-rs/` and is published as `mtp-rs`. A companion CLI binary lives in `crates/mtp-rs-cli/` and is published as `mtp-rs-cli` (the installed binary is named `mtp-rs`).

## Quick commands

- `just`: Run all checks: format, lint, test, doc
- `just fix`: Auto-fix formatting and clippy warnings
- `just check-all`: Include MSRV check, security audit, license check, and `just semver` (accidental API breaks)
- `just clippy-stable`: Lint with `unstable-raw` off (part of `just`)
- `just release-dry`: `cargo publish --dry-run` for both crates
- `cargo test --workspace --all-features`: Run with proptest fuzzing across the workspace
- `just fuzz <target> [seconds]`: Fuzz one PTP parse entry point with cargo-fuzz (nightly); `just fuzz-seeds` regenerates the committed seeds

## Project structure

```
crates/
  mtp-rs/                    # Library (crates.io: mtp-rs)
    src/
      mtp/                   # High-level API (MtpDevice, Storage)
      ptp/                   # Low-level protocol (PtpDevice, PtpSession)
        codes.rs             # OperationCode, ResponseCode, EventCode
      transport/             # USB abstraction (Transport trait, nusb, mock, virtual_device). Private
                             # without the `unstable-raw` feature
      virtual_device.rs      # Public home of the virtual-device config and test hooks
      fuzzing.rs             # Parse entry points for `fuzz/` (the `fuzzing` feature), plus the seed corpus
    fuzz/                    # cargo-fuzz targets, own workspace; read its CLAUDE.md before adding one
    tests/integration.rs     # Real-device tests
    tests/stable_api.rs      # What a consumer can build and read from outside the crate
    examples/                # list_and_download, ptp_diagnose, fuji_capture, etc.
  mtp-rs-cli/                # CLI binary (crates.io: mtp-rs-cli, binary: mtp-rs)
    src/
      main.rs                # Entry point
      cli/                   # Subcommand dispatch, args, error mapping, paths
    tests/cli.rs             # Cross-process tests via the built binary + virtual device
    docs/cli.md              # Full command reference
benchmarks/
  mtp-rs-vs-libmtp/          # Throughput vs libmtp comparison (not published)
docs/                        # Protocol, architecture, debugging, release process
```

## Architecture

```
mtp:: (MtpDevice, Storage, FileDownload, ObjectListing)   <-- backend-neutral high-level API
  |  Box<dyn MtpBackend>  (mtp::backend, pub(crate))
  +-- UsbBackend (mtp::backend::usb): PTP over Transport (nusb | virtual | mock)
  +-- WpdBackend (mtp::backend::wpd, cfg(windows)): WPD over COM
ptp:: (PtpSession)            <-- Cameras, protocol work (USB-only by nature)
  |
transport:: (Transport trait)
  |
nusb (USB)  or  VirtualTransport (filesystem, feature = "virtual-device")
```

The `mtp::` layer is **backend-neutral**: it speaks neutral types (`mtp::{ObjectHandle, StorageId,
ObjectInfo, ObjectFormat, DeviceInfo, StorageInfo, Capabilities, DateTime, UtcOffset}`) and the neutral
`mtp::Error`, and dispatches through the `MtpBackend` trait. `MtpDevice`/`Storage` are thin façades
over `Box<dyn MtpBackend>`; `UsbBackend` holds the `PtpSession`,
converting PTP↔neutral only at its boundary (via `to_ptp`/`from_ptp` on the neutral types and
`From<PtpError> for mtp::Error`). All device-quirk logic (root-listing fast path, Android/Samsung/Fuji
fallbacks, >4 GB size resolution, SIC cancel, recovery, the upload partial-handle contract) lives in
`UsbBackend`. The virtual device and mock are **not** separate backends — they're `Transport`s under
`UsbBackend`, so every existing test exercises the real backend path. `WpdBackend` is the second
`MtpBackend`, on Windows; see § Windows WPD backend.

**Errors:** `mtp::Error` (re-exported as the crate-root `Error`) is the neutral high-level error with
backend-agnostic variants (`NotFound`, `StaleHandle`, `AccessDenied`, `Unsupported`, `Busy`,
`StorageFull`, `Cancelled`, `Disconnected`, …). The rich low-level PTP error is `ptp::PtpError`, which
keeps detailed response codes for camera/protocol users. Its `Usb` variant holds an opaque `ptp::UsbError`,
so no `nusb` type reaches a stable signature; build one with the crate-private `PtpError::from_nusb`, and
don't add a `From<nusb::Error>`, since a public trait impl would put `nusb` back in the API.
**PTP strings cap at 254 UTF-16 units**: `pack_string` is fallible and the `Storage` façade refuses a
longer name with `Error::NameTooLong` before any backend sees it; never truncate on the host side.
The virtual device truncates like AOSP instead. Why and how: `docs/protocol.md` § Strings.

**Entry points:** `MtpDevice::open_first()`, `PtpDevice::open_first()`, `MtpDevice::list_devices()` and `PtpDevice::list_devices()` (enumerate, then open by serial or location; the PTP one is USB-only), `watch_devices()` (hotplug stream), `MtpDevice::reset_by_serial()` (session-less transport reset),
`MtpDeviceBuilder::open_virtual()` (feature-gated), `MtpDeviceBuilder::reuse_existing_session(id)`
(USB-only opt-in for a stable session that survives across host processes)

**Key types:** `ObjectHandle`, `StorageId` (opaque `u64` newtypes, session-scoped tokens — not wire
values), `ObjectFormat` (raw MTP format code + category helpers), `Capabilities` (replaces the old
`is_android`/`supports_*` accessors; `MtpDevice::capabilities()`, plus convenience
`supports_rename()`/`supports_upload()`), `ByteRange { Full, From(off), Range { offset, len } }`
(drives `download`/`download_with_pacing`), `DownloadPacing`, `UsbSpeed` (negotiated USB link speed on
`MtpDeviceInfo::speed` / `UsbDeviceInfo::speed`; both info structs are `#[non_exhaustive]`).

**Download API:** streaming `Storage::download(handle, range)` returns a `FileDownload` that reads in
windows of about two seconds and frees the session between them; `download_with_pacing` takes a
`DownloadPacing` (`HeldOpen`, `MaxCancelDelay`, `FixedWindow`). See § Paced downloads. Buffered:
`Storage::download_to_vec(handle)` / `Storage::read_range(handle, offset, len)` /
`Storage::thumbnail(handle)`.

## Public surface and stability

The release that shaped this is 0.37.0 (#34). The goal: a 1.0 that's only a matter of waiting.

- **Stable core**: what the crate root re-exports (the backend-neutral `mtp::` API, `CancelToken`, `UsbSpeed`,
  `MtpMatchReason`, `MtpDeviceInfo`), plus `ptp::` in its first-class shape. **The root path is the promise**; nested
  `mtp::` paths work too. Every new stable item gets a root re-export.
- **Exempt from SemVer**: the `unstable-raw` feature (`transport::`, `MtpDeviceBuilder::open_nusb_device`,
  `PtpSession::open`, `PtpSession::open_reusing_existing`), the `virtual-device` feature (`mtp_rs::virtual_device`,
  `open_virtual`), the `fuzzing` feature (`mtp_rs::fuzzing`, doc-hidden, for `fuzz/` only), the `tracing` feature's
  event names and fields, and log output.
- **Nothing `nusb`-typed in a stable signature**, including errors and trait impls. `nusb` is 0.x, and its next release
  must not be able to force an mtp-rs major. Nothing in this repo needs `unstable-raw`: if a test, example, or the CLI
  starts to, or a consumer does, that's a gap in the stable core, so widen the core.
- **`ptp::` keeps wire formats private.** `pack`, `CommandContainer`, `DataContainer`, and dataset `from_bytes`/`to_bytes`
  are `pub(crate)`. Public only where a signature needs them: `ResponseContainer` and `EventContainer` as readable
  values, `PropertyValue::{to_bytes,from_bytes}` (the raw-bytes property calls), and
  `ObjectPropListElement::list_from_bytes` (`get_object_prop_list` returns a raw dataset). `mtp::` names no `ptp::` type
  in a signature except the `From` conversions, and `UtcOffset`, which is shared.
- **`#[non_exhaustive]` on anything the library produces or a spec can extend.** Caller-built types need a way in:
  constructors or `Default` plus field assignment. `tests/stable_api.rs` pins that from outside the crate, the only place
  the attribute bites. Deliberate exceptions: `VirtualDeviceConfig` (see CONTRIBUTING.md), `HotplugEvent` (no third state),
  `ListingItem` (a new kind must be a compile error, or it could vanish from a listing), and the ID newtypes.
- **Promoting `ptp::` at 1.0 is decided by data, not now.** During the quiet period, track whether `cargo-semver-checks`
  or the changelog shows a `ptp::` break. If `ptp::` stayed quiet, it's stable in 1.0. If not, 1.0 ships with `ptp::`
  behind a feature, and a later 1.x promotes it. Feature-gated to stable is additive; the other direction is breaking.
- **When to call it 1.0**: the clock restarted with 0.38.0 (2026-11-06, the `mtp::` download API, #60), the last breaking release so far; a later break restarts it. `ptp::` has had no break since 0.37.0 (2026-10-31). Track weeks since the last breaking change, per layer (`mtp::` and `ptp::`). At about 8–12
  weeks, release 1.0, possibly through a `1.0.0-rc.1`.

Policies (the README's "Stability" section says the same to users):

- **Patch for pure fixes**: a fix that changes no API ships as a patch, so `^0.38` users get it through `cargo update`.
  This matters for data safety: fixes that only land in a minor strand people on the old bug. Minors are for features
  and, before 1.0, breaking changes.
- **Changing a default** (timeouts, window sizes) isn't breaking. Put it in the changelog.
- **MSRV**: Rust releases from the last 12 months. A raise is a minor bump with a changelog line, not a SemVer break. See
  [docs/dependency-updates.md](docs/dependency-updates.md).
- **Security fixes** may change behavior in a minor. Latest release only, no backports. See [SECURITY.md](SECURITY.md).
  Data-safety bugs don't go to RustSec.

`just semver` compares the default-feature surface against the latest crates.io release, so the exempt features stay out
of it. It reads the version in `Cargo.toml`: a deliberate break passes once the version says so, and fails otherwise.

## Known device quirks

- **Android**: `ObjectHandle::ALL` recursive listing broken. Listings don't detect Android; every root listing takes
  the path below. The one Android check is for cancels, `DeviceInfo::runs_android_mtp_server` (§ Transfer
  cancellation), and it can't rely on `android.com` in the vendor extension: Samsung leaves it out.
- **Android/Kindle root listings** ask `parent=0xFFFFFFFF` first (it returns root-level handles, where `parent=0`
  returns every object on the storage) and fall back to `parent=0` only when the device **declines**: `Protocol`, or
  `Io`, which is how a camera's bulk STALL arrives (`is_all_handle_rejection` in `mtp::backend::usb`). Don't widen that
  back to a catch-all. `DeviceReset`, `Timeout`, and `Disconnected` must propagate: the retry hammers a device that
  re-wedges under exactly that treatment (#18), and it reports the *second* error, hiding the `DeviceReset` a consumer
  needs to trigger its reopen.
- **Some responders report the containing storage ID as the parent of root objects**, not `0` or `0xFFFFFFFF` (DBI and
  Sphaira/libhaze on the Nintendo Switch, PR #20). The root filter accepts it, but only after `root_filter` in
  `mtp::backend::usb` checks the storage ID isn't itself one of the enumerated handles. Keep that guard: a storage ID is
  a small number (`0x00010001` is 65,537), and on the recursive fallback path the handle set covers the whole storage,
  so a device with a large library can genuinely own a folder at that handle. Without the guard, `parent == storage_id`
  reads as "at the root" for that folder's children too, and they get listed alongside the real root entries (a tree
  walk then visits their subtree twice; nested listings are unaffected, they use `Exact`).
- **Root writes address the storage root as `0xFFFFFFFF`, not `0`.** `SendObjectInfo`'s parent parameter is the one
  place MTP spells the root that way (MTP 1.1 D.2.12); `0` means "no parent given, responder may choose", so responders
  look it up as a handle and reject it. Android's `MtpServer` only maps `MTP_PARENT_ROOT` (0xFFFFFFFF) to the storage
  path, and libhaze (the Nintendo Switch homebrew responder behind Sphaira, #21) only maps 0xFFFFFFFF to its storage
  object; both answer `0` with `InvalidObjectHandle`. `PtpHandle::SEND_ROOT` carries this, and `upload`/`create_folder`
  use it when the caller passes no parent. **The spec is asymmetric, so don't unify it with `PtpHandle::ROOT`**:
  `MoveObject`/`CopyObject` (D.2.25/D.2.26) spell the same destination `0x00000000`, and
  `move_and_copy_keep_handle_zero_for_the_root_destination` in `mtp::backend::usb` pins that down. The virtual device
  normalizes `SEND_ROOT` to `ROOT` on receive and still accepts `0`, since low-level `ptp::` callers may send either.
  **Hardware-verified by A/B on a Pixel 9 Pro XL** (macOS/nusb, 2026-08-10): the pre-fix binary failed both
  `mkdir /<name>` and a root `put` with `InvalidObjectHandle` (surfacing as "remote path not found"), and the fixed
  binary created a root folder, uploaded a root file, verified it, and deleted both. So the old "Android can't create in
  the storage root" line in these docs was describing this bug, not Android.
- **Android**: Object handles are NOT stable. MediaProvider re-keys object IDs across a media rescan, so a handle a host cached when it last listed a folder can be silently invalidated before a later operation (upload, delete) into that folder: the device then returns `InvalidObjectHandle`/`InvalidParentObject`, not for a missing object but for a stale ID. Hosts should treat those codes on a previously-valid handle as "re-list the parent and re-resolve, then retry once", not as a hard not-found. (A downstream, Cmdr, hit this as a 307 MB upload failing at `SendObjectInfo` and surfacing as "Path not found" on the intact *source* file.) Reproducible against the virtual device with `rekey_virtual_object` (see Testing).
- **Android answers a `CopyObject` into a folder that already holds the name with OK and makes no copy** (#59). Read
  from AOSP `main` on 2026-11-04: `MtpStorageManager.beginCopyObject` refuses with -1, `android_mtp_MtpDatabase.cpp`
  passes it back as a 16-bit `MtpResponseCode` (0xFFFF), and `MtpServer::doCopyObject` compares that against the
  32-bit `kInvalidObjectHandle`, misses, and runs the copy anyway. `copyFile` (`MtpUtils.cpp`) opens the destination
  without `O_TRUNC`, so a same-folder copy rewrites the file onto itself and a copy onto a same-named file elsewhere
  overwrites it in place, keeping its tail if it was longer. A folder merges into the existing one (`GeneralError`).
  The response carries handle 0xFFFF; WPD passes the OK through. `MtpStorageManager` matches names exactly while shared
  storage is case-insensitive, so `a.jpg` onto `A.JPG` gets past the check with a real handle and overwrites too.
  Hence three guards, keep all of them: `Storage::copy_object` refuses a destination holding the name (case
  ignored) with `Error::AlreadyExists`; `UsbBackend::verify_copy` checks the answered handle with `GetObjectInfo`;
  WPD's `copy_object` returns only an object that wasn't in the destination before (it used to match the name and
  return the original, which an app then renamed). The virtual device models AOSP's answer.
  **Hardware-verified on a Pixel 9 Pro XL** (macOS/nusb, 2026-11-04, raw `ptp::` `CopyObject`): a same-folder copy
  answered OK + 0xFFFF and made nothing, and a copy of a 6-byte `a.txt` onto a 25-byte `a.txt` in another folder
  answered the same and left `SHORT\n` followed by the old file's last 19 bytes. So it's stock Android, not only the
  POCO. Reported upstream as https://issuetracker.google.com/issues/581487246, with patches up for review (links in
  `docs/notes/community-threads.md` § #59); old Android keeps the bug either way. The CLI's `cp` makes a renamed copy
  next to the original through a hidden staging folder (copy, rename, move back, remove the folder).
- **Xiaomi POCO X7 Pro (HyperOS 3)** declines `GetObjectPropList` for "all properties" (`0xFFFFFFFF`) with
  `InvalidObjectPropCode` (0xA801) in most folders but answers each property alone, and sends a property it has no
  value for (`DateCreated`) with data type 0 and no value. We never ask "all"; see § Folder listings via
  `GetObjectPropList`.
- **Android's `GetObjectPropList` has no creation date**: AOSP's file property set (`FILE_PROPERTIES` in
  `MtpDatabase.java`) leaves out `DateCreated`, and asked alone it comes back Undefined. Its `GetObjectInfo` reports
  `DateCreated` equal to `DateModified`, so fast-path listings there have `created: None` where per-handle listings
  had a copy of the modification time. Deliberate: we don't synthesize it.
- **Fujifilm cameras**: Report `AccessCapability::ReadWrite` but return `StoreReadOnly` on writes. Advertised ops lie.
- **Samsung**: Returns `InvalidObjectHandle` for root listing; needs recursive traversal with filtering
- **Panasonic Lumix DMC-TZ61** (and likely other PTP cameras): Reports `20480000T000000` (month 0, day 0) as "no date" in ObjectInfo datetimes. Receive-side datetime parsing is lenient for this reason: unparseable datetimes become `None` instead of failing the dataset parse. Send-side packing stays strict.
- **Datetimes are usually zoneless, and that's kept honest.** `DateTime::offset` is `None` unless the string ended in `Z` or `±hhmm`; `None` means wall-clock time in the device's zone, never UTC, so `to_unix_seconds()` answers only with an offset and `to_unix_seconds_with_fallback(zone)` makes the consumer pick one. Android writes phone-local time with no zone and, on receive, honors `Z` but reads any `±hhmm` as phone-local (verified against AOSP `frameworks/av` `media/mtp/MtpUtils.cpp` `parseDateTime`/`formatDateTime` on `main`, 2026-10-07). Parsing is exact (real month lengths, leap years, digits only); an unrecognized suffix yields `None` rather than a zoneless value, since it might be an offset we'd be ignoring.
- **Teenage Engineering TP-7**: Keeps its device-side MTP session open across host processes and treats
  `CloseSession` as a request to leave MTP mode. Use
  `MtpDeviceBuilder::reuse_existing_session(0xBAAA_AAAD)` to reuse its stable session without sending
  `CloseSession` when `OpenSession` reports `SessionAlreadyOpen`. Dropping `MtpDevice` leaves the session
  alone; explicitly calling `MtpDevice::close()` still sends `CloseSession` and exits MTP mode.
  **Hardware-verified A/B on TP-7 firmware 2.5.7 / MTP 1.1.11** (macOS/nusb, 2026-08-27): with
  session reuse disabled, the first CLI process succeeded and the second found the recorder back in
  `audio-midi` mode; with reuse enabled, two consecutive CLI processes both succeeded and left it in
  `mtp` mode.
- **macOS reports a pulled cable as two unmapped IOKit codes**: `kIOReturnNotResponding` (`0xe00002ed`) on the
  in-flight transfer and `kIOReturnNotOpen` (`0xe00002cd`) on the next, which nusb 0.2.8 leaves as
  `TransferError::Unknown`. `is_disconnect` in `transport/nusb.rs` folds both into `PtpError::Disconnected`; any site
  that classifies a `TransferError` must ask it rather than match `TransferError::Disconnected` (Pixel 9 Pro XL, macOS
  27.0.1, cable pulled mid-upload, 2026-10-30, #53).

## Testing

> **Debugging against real hardware? Read [docs/debugging.md](docs/debugging.md) first.**
> It's the debugging hub: how to keep macOS's `ptpcamerad` off the interface (run
> the blocker yourself), recover a wedged device (quiet spaced reopens first,
> `mtp-rs reset` last: on Android the reset can break MTP until a replug), fail
> fast with `MTP_TEST_TIMEOUT_SECS=2`, and the Android gotchas. Only needed when a
> physical device is involved.

- **Unit**: `cargo test --workspace` (uses mock transport)
- **Fuzz**: `crates/mtp-rs/fuzz/`, one cargo-fuzz target per device-facing parser, run weekly by `.github/workflows/fuzz.yml`. The proptest `fuzz_*` tests stay as the quick in-suite version. See `crates/mtp-rs/fuzz/CLAUDE.md`.
- **Filesystem-watcher tests run in their own pass** (`just test` does this for you: everything else, then `fs_watcher` with `--test-threads=1`). They wait on real OS filesystem-event delivery, so inside the ~400-test parallel pool a loaded machine starves them past their poll budget and they fail as a group while passing every time alone. Don't fold them back in, and don't "fix" a flake there by inflating the timeout. Each one also calls `wait_for_watcher_ready` first, which writes a probe file and waits for the watcher to report it: `notify` arms its stream on a background thread, so a write issued straight after `open_virtual` can land before anything is listening. The probe file is deliberately left in place, since deleting it queues a late `ObjectRemoved` that lands after the drain and steals the next test's first event.
- **Virtual device**: `cargo test -p mtp-rs --features virtual-device` (full protocol tests against local filesystem). Everything public is in `mtp_rs::virtual_device`. `VirtualDeviceConfig` implements `Default`, so build it as `VirtualDeviceConfig { storages: vec![...], ..Default::default() }` and state only the fields a test actually exercises; new fields must land with a default so consumers don't break (see CONTRIBUTING.md). `VirtualStorageConfig` has no `Default` (an unset `backing_dir` fails silently). Bulk reads honor the requested size like USB, so a held-open download or a window over 64 KiB comes out of `next_chunk` in 64 KiB pieces, and a consumer can test its per-call memory bound against it (#63). Fault injection: `force_partial_read_caps` (short/stall reads), `force_cancel_wedge` / `force_operation_wedge` (#18), `force_object_info_error` and `VirtualDeviceConfig::undescribable_objects` (partially-readable folders, #22). Capability shaping: `supports_partial_object` / `supports_partial_object_64` pick which partial-read ops the device advertises AND serves (both `false` models libhaze/Sphaira, where `download(ByteRange::Full)` is the only read). `GetObjectPropList` is advertised and served by default (modern Android does), so every non-root virtual-device listing takes the fast path; `supports_object_prop_list: false` turns it off, and `prop_list_declined_properties` declines single properties. It models AOSP (depth 1 includes the folder itself, unknown properties come back Undefined) except that it serves `DateCreated` alone, so both listing paths match field for field. A prop-list answer that would include an `undescribable_objects` / `force_object_info_error` object fails whole with `GeneralError`, so #22 skips still happen on the per-handle fallback. Dates: objects report their backing file's mtime (and birth time) on the clock set by `utc_offset` (default UTC, zoneless unless `dates_include_offset`), and an upload's `DateModified` becomes the file's mtime, so a fixture seeds a date with `File::set_modified`. `android` (default `true`) models Android's `MtpServer`: it advertises `android.com`, so the host drains instead of sending CLASS_CANCEL and ends a stopped upload with a zero-length packet, and the device drops the half-written object (`UploadError::partial` is `None`, #62). `android: false` models a PTP camera: CLASS_CANCEL, and a stopped upload keeps its partial. Tests named `*_on_a_camera` cover that path.
- **Integration**: `cargo test -p mtp-rs --test integration -- --ignored --nocapture` (needs device). Destructive tests pick a writable root folder from a priority list (Android `Download`, Garmin `Music`, Kindle `documents`, etc.); set `MTP_TEST_FOLDER=Name` to override. A timeout or missing device skips a test only until the device has answered once in the run; after that it fails, so a wedge mid-run can't pass as a row of skips (#43). See `crates/mtp-rs/tests/integration.rs` header for full details. To compare the two listing paths on a real device (read-only, prints timings): `MTP_TEST_LIST_FOLDER=/DCIM/Camera cargo test -p mtp-rs --test integration prop_list -- --ignored --nocapture`, or `cargo run --release -p mtp-rs --example list_speed -- DCIM/Camera`.
- **CLI**: `cargo test -p mtp-rs-cli --features virtual-device` (runs the built binary against a virtual device)
- **Property**: `cargo test --workspace --all-features` (proptest fuzzing)
- **Stable API**: `tests/stable_api.rs` builds and reads the stable types from outside the crate, with no features
- **Windows from a Mac**: `cargo clippy --target x86_64-pc-windows-msvc -p mtp-rs --all-targets --all-features -- -D warnings` type-checks the WPD backend locally, which CI otherwise only does on push

## Design principles

- **Pure Rust**: No C/FFI, no `-sys` crates
- **Runtime-agnostic**: `futures` traits only, no tokio/async-std dependency
- **Stream-based**: Downloads and uploads stream via `Stream<Item = Chunk>`. Peak memory per transfer is about one
  64 KiB USB read, whatever the file's size (see "Receiving data containers" below)
- **Safe cancellation**: Mid-stream downloads can be cancelled via USB SIC class cancel
- **Type-safe handles**: Newtypes prevent ID mixups

## Watching for devices arriving and leaving

`mtp::watch_devices()` returns a `DeviceWatch`, a `Stream<Item = HotplugEvent>`
(`Arrived(MtpDeviceInfo)` / `Left(MtpDeviceInfo)`) driven by `nusb::watch_devices()`.
`DeviceWatchBuilder` tunes it (`known_devices` mirrors `list_devices_with_known`,
`settle_delay` overrides `DEFAULT_SETTLE_DELAY`, 500 ms). USB only: virtual devices are
registered in-process, so they never produce events even though `list_devices` includes them.
`examples/watch_devices.rs` needs hardware; the diff logic is unit-tested in `mtp/hotplug.rs`.

Three decisions worth not undoing:

- **Every USB event triggers a fresh `list_mtp_devices_with_known()` and a diff against the
  last known set; the event's own payload is only a trigger.** The payload can predate the
  device's descriptors being readable, so classifying from it silently drops real devices; and
  a `Disconnected` event carries just an opaque `nusb::DeviceId`, which can't be matched to a
  serial at all. Re-enumerating costs one syscall sweep per USB event and makes both directions
  correct. It's also what lets `Left` carry the full `MtpDeviceInfo`: consumers need the serial
  to know *which* phone left, and only the cache has it.
- **The settle delay exists because arrival detection is otherwise racy**, not for debouncing
  (coalescing is a side benefit: events during the delay fold into one enumeration). Cmdr hit
  this first and hand-rolled the same 500 ms wait.
- **Devices already connected are emitted as `Arrived` on first poll.** One consumer code path
  instead of enumerate-then-watch, and no gap for a device plugged in during startup to fall
  through. Consumers therefore must not enumerate separately, or they'll double-count.

Identity is keyed on `(location_id, vendor_id, product_id, serial_number)`, not `location_id`
alone: swapping phones between two enumerations, or an Android phone re-enumerating from
charge-only into file transfer (new product ID), must read as `Left` then `Arrived`. A failed
enumeration keeps the last known set rather than reporting everything as departed.

## Folder listings via `GetObjectPropList`

`UsbBackend::list` lists a **non-root** folder with `GetObjectPropList` (0x9805, depth 1) when the device advertises
it, instead of `GetObjectHandles` plus one `GetObjectInfo` per child. On Android that's the difference between seconds
and minutes for a big photo folder: AOSP's `GetObjectInfo` opens every JPEG/HEIF to parse EXIF (`getObjectInfo` in
`android_mtp_MtpDatabase.cpp`), while its prop list walks an in-memory file list. Contributed in PR #45, then
hardened. Root listings always stay per handle (they carry the root-filter quirks and are short). WPD is unaffected.
Consumers can turn it off with `MtpDeviceBuilder::use_object_prop_list(false)`; the CLI has `--no-prop-list`.

The policy lives in `UsbBackend::list_by_prop_list`; `ptp::` only has the spec-shaped
`PtpSession::get_object_prop_list` and the dataset parser `ObjectPropListElement::list_from_bytes`. Keep it that way.

- **One request per property, never "all properties" (`0xFFFFFFFF`).** The required four first (`ParentObject`,
  `ObjectFileName`, `ObjectFormat`, `ObjectSize`), then the optional dates (`DateModified`, `DateCreated`). StorageId
  isn't asked: the listed storage fills it in. A date the device declines or never gives a value for (Android answers
  `DateCreated` Undefined) is remembered for the session. The session state only moves one way: on, then off.
- **Why not "all"**: AOSP picks the "all properties" group from the *first child's* format and reuses it for every
  object (`getObjectPropertyList` in `MtpDatabase.java`), so whether it works and what it costs depend on which file
  happens to come first. On a Pixel 9 Pro XL (macOS/nusb, 2026-10-23), `/Pictures/Screenshots` (726 PNGs) spent 3.2 s
  on "all" and then got 0xA801 (`InvalidObjectPropCode`); the per-property requests that followed took ~50 ms each
  (~0.25 s), while the per-handle listing of that folder is ~0.9 s. In `DCIM/Camera` (JPEGs) "all" worked. The Xiaomi
  declines it too. With 10k objects a failing "all" could take tens of seconds or hit the 30 s timeout, so don't
  bring it back as a first try.
- **Never a silent wrong listing.** Every child must carry the required properties (size for files only; a folder's
  size means nothing), and every `ParentObject` must equal the requested folder. The folder itself (depth 1 includes
  it) is dropped by handle, never by a parent filter: a filter is how a device that omits `ParentObject` would show up
  as an empty folder, which `AGENTS.md` § Partially-readable folders forbids. A required property missing from every
  child, or a mismatched parent, means the device can't do this: off for the session. Missing from only some children
  (an object created mid-listing) falls back for that listing only.
- **An empty answer falls through to the per-handle path**, which confirms with one `GetObjectHandles`. One extra
  request per empty folder buys protection from a device that treats depth 1 like depth 0.
- **Two kinds of refusal** (`is_prop_list_capability_decline`, next to `is_all_handle_rejection`). Capability
  declines (`OperationNotSupported`, `ParameterNotSupported`, 0xA801 `InvalidObjectPropCode`, 0xA802
  `InvalidObjectPropFormat`, 0xA807/0xA808 `SpecificationBy{Group,Depth}Unsupported`, and `Io`, a camera's STALL)
  turn it off for the session. Every other response code (`GeneralError`, `InvalidObjectHandle`, `AccessDenied`,
  `DeviceBusy`, …) is about this folder only: fall back once, keep it on. Don't merge the two: Android re-keys
  handles, so a stale parent would otherwise switch the fast path off for the whole session, and so would one #22
  folder.
- **Session failures propagate**, exactly like the root fast path: `Timeout`, `Disconnected`, `DeviceReset`, and a
  transaction-ID desync (which surfaces as `InvalidData` from the session, so only a dataset that fails to *parse*
  counts as unusable) must not be answered with one `GetObjectInfo` per child.
- **What this path doesn't carry**: image pixel dimensions (`ObjectInfo::image_width/height` stay 0; AOSP fills them
  from EXIF in `GetObjectInfo` only), the association type (folders are detected by the Association format), and skips
  (a #22-style refusal fails the whole request and the per-handle fallback produces the skip). The listing arrives in
  one piece, so the `CancelToken` is checked between requests, not between objects.
- **Hardware evidence**: Xiaomi POCO X7 Pro (HyperOS 3, Linux/nusb, 2026-10-22, contributor's run of the original
  PR): 1,938 objects, fields identical to `GetObjectInfo`, 0.67 s against 7.3 s. With the per-property-only policy
  (macOS/nusb, 2026-10-23, `examples/list_speed.rs`, per-handle side timed warm): Pixel 9 Pro XL, 274 photos 0.24 s
  against 2.2–2.8 s and 726 PNGs 0.18 s against 0.92 s; Galaxy S23 Ultra SM-S918B (One UI firmware S918BXXSAFZI1),
  3,248 photos 0.40 s against 5.5 s and 1,916 screenshots 0.18 s against 2.6 s. Every field identical on both phones
  apart from Android's `created` (None) and image size (0). libmtp marks every Android device
  `DEVICE_FLAG_BROKEN_MTPGETOBJPROPLIST`, so there's little field mileage elsewhere: check new devices with the
  comparison test (§ Testing).

## Partially-readable folders (tolerant listing)

A device can hand back a folder's handle list and then refuse to describe one of them. Sphaira
(Nintendo Switch homebrew) does this for one handle out of 50 (#22). One rejected `GetObjectInfo`
must not hide the other 49.

**What may be skipped.** All three have to hold, and the rule lives on `Storage::collect_objects`:

1. The handle list is already in hand, so the folder's membership isn't in doubt, only one entry's
   metadata.
2. The failing operation is read-only, so nothing on the device changed.
3. The device answered with a protocol response code, which closes that transaction cleanly and
   leaves the session usable for the next handle.

Exactly one case qualifies today: `GeneralError` on `GetObjectInfo`. **Don't add codes
speculatively** (`AccessDenied` would satisfy the rule, but no device has been observed doing it).
Adding one is a one-line change in `UsbBackend::list` once a device justifies it.

**Everything else stays fatal**: transport and session failures, malformed responses, cancellation,
stale handles, and any failure to enumerate the handles in the first place.

**All-skipped is a failure, not an empty folder.** If a non-empty handle list yields zero successes,
`collect_objects` returns the first error rather than `Ok(vec![])`. Don't "simplify" this away: an
empty Vec renders as an empty folder in a file manager and reads as "everything was deleted" to
anything syncing, so it turns a read failure into data loss. All-or-nothing rather than a
percentage, because a threshold would be arbitrary and this isn't: we learned nothing about a folder
we know has contents. A genuinely empty folder (no handles) stays an empty folder.

**The two APIs agree.** `ObjectListing::next` yields `Result<ListingItem, Error>` where `ListingItem`
is `Object` or `Skipped`, so `Err` keeps one meaning ("the listing is over") and a skip can't be
mistaken for a fatal error. `collect_objects` returns `ObjectCollection { objects, skipped }`;
`list_objects` is the same read with `skipped` dropped. Keep the streaming and collecting halves in
sync: if one learns a new distinction, so does the other.

**Consumers**: `mtp-rs ls` prints a stderr warning in both plain and `--json` modes and always emits
a `skipped` array (empty when nothing was skipped, so a script reads one field unconditionally);
`mtp-rs get` on a folder does the same for the objects it couldn't download; `doctor` reports `unreadable_root_objects`; `collect_objects_recursive` aggregates skips across a
whole tree walk.

**Testing it**: `force_object_info_error(serial, handle, code)` on a virtual device (by handle, in
process, armed after a listing), or `VirtualDeviceConfig::undescribable_objects` (by storage-relative
path, up front, for consumers testing their own binary out of process, for example a CLI or a FUSE
mount). The object stays present and readable by every other operation either way, which is what
makes it a model of the real device rather than a deletion.

**WPD**: the Windows backend's per-child listing reader is lenient (an unreadable child degrades to a
default record), so it never produces skips and `skipped` is always empty there. That asymmetry is
deliberate but undocumented in the WPD plan; don't assume parity.

## Cooperative cancellation for list/delete ops

`Storage::list_objects_with_cancel`, `list_objects_stream_with_cancel`, and
`delete_with_cancel` take an `Option<&CancelToken>`. The token is
`Arc<AtomicBool>`-backed, cheap to clone, and one-way (no reset; make a fresh
token per logical op). When set, `ObjectListing::next` checks before issuing
each `GetObjectInfo` USB roundtrip and bails with `Err(Error::Cancelled)`.

If you already have an `Arc<AtomicBool>` driving cancellation on the consumer
side (a write-operation intent flag, a shared abort signal, anything), use
`CancelToken::from_arc(arc)` to wrap it without a second polling task. The
constructor shares the atomic, so flipping the consumer-side bool also flips
the token; `Default::default()` builds a fresh one from scratch.

For per-handle list/delete this is sufficient and safer than mid-USB-transaction
cancel: each `GetObjectInfo` and `DeleteObject` roundtrip completes in
milliseconds, so there's no half-finished transfer to drain. The CancelToken
short-circuits the per-handle for-loop, which is where 1k-entry Android folder
listings spend their 15+ seconds when they can't use `GetObjectPropList` (see
§ Folder listings via `GetObjectPropList`; on that path the token is checked
between the few per-property requests).

Streaming downloads keep using the SIC class-cancel path (see below); that's a
different mechanism for a different problem (one big bulk-IN to drain).

## Transfer cancellation

Mid-stream download cancellation uses the USB Still Image Class (SIC) cancel
mechanism: a CLASS_CANCEL control request (bRequest=0x64) followed by draining
the bulk IN and interrupt pipes. This approach was validated against libmtp's
`ptp_read_cancel_func` (Florent Viard, 2017). Key implementation notes:

- **Android gets no CLASS_CANCEL at all** (#54): `cancel_transfer` drains the
  rest of the object instead, the same path a refused cancel takes. A
  CLASS_CANCEL races `MtpServer`'s own `io_cancel`, and losing that race ends the
  server until a replug, about one cancel in ten on a Pixel 9 Pro XL and a Galaxy
  S23 Ultra; the reset didn't revive the S23 either. Draining: no wedge in 150+
  runs on each phone, explicit cancels and dropped futures alike. Cost: the rest
  of the transfer (~2.5 s per 700 MB on the Pixel). `GetDeviceInfo` decides, in
  `DeviceInfo::runs_android_mtp_server`, through `Transport::set_drain_on_cancel`.
  **Samsung's vendor extension has no `android.com`**, so the check also accepts
  AOSP's edit operations (0x95C3–0x95C5). Cameras keep CLASS_CANCEL (#12).
  Evidence: [docs/notes/android-wedges-and-the-reset-kill-switch.md](docs/notes/android-wedges-and-the-reset-kill-switch.md).
- The drain must start **immediately** after CLASS_CANCEL: any delay (like
  polling GET_DEVICE_STATUS, which Android doesn't support) allows the device
  to enter an unrecoverable state.
- The drain uses maxpacket-sized reads with a 300ms idle timeout (matching
  libmtp and Windows behavior).
- **Some devices refuse CLASS_CANCEL with a STALL** (Xiaomi POCO X6 Pro on
  Linux, #43; its sibling X7 Pro accepts it). The device then keeps sending the
  object, so `cancel_transfer` drains anyway, in 64 KiB reads, until the
  closing Response arrives: the cancel costs the rest of the transfer, but the
  session stays in sync. Returning the STALL as an error instead left the rest
  of the object in the pipe and wedged the phone until a replug.
- **A CLASS_CANCEL that times out takes the same path as a refused one** (`cancel_was_not_acted_on`). The
  status poll after the drains then decides: a device that's silent there too is the wedge below and gets
  `Error::DeviceReset`. Returning `Timeout` right away skipped that check, so the caller never learned to
  reopen (Galaxy A57 on Linux, a second cancel soon after a reset, #43; from the code and the 0.3 s timing,
  not confirmed on hardware yet).
- **The drain recognizes the Response only as a short read whose length field
  matches** (`is_response_container`). File data has `03 00` at bytes 4..6 in
  about one read in 65,536, so a drain through a whole object that checked only
  the type code would stop early with data still queued.
- **A failed `ReceiveStream::cancel` flags the session for recovery** (except
  on `DeviceReset`, where the caller must reopen quietly), so the next
  operation drains instead of reading leftover bytes as its response.
- The interrupt pipe must also be drained: some devices (GoPro) freeze if
  the CancelTransaction event is left unread.
- **After** the drains, GET_DEVICE_STATUS (0x67) **must** be polled until the
  device stops reporting Device_Busy, clearing any endpoint halts the status
  reports. SIC-compliant cameras wait for this before accepting new
  operations; skipping it left the Lumix DMC-TZ61 (#12) dead after every
  cancel. The order is the whole trick: drain first (for Android), poll after
  (for cameras). A healthy Android device answers this poll `OK` or fails it
  harmlessly; the poll is not what wedges the Samsung below (that was ruled out
  on hardware).
- **Interrupting an in-flight bulk read wedges Android devices** (Galaxy S23
  Ultra, qarmin's A15, #18, and a Pixel 9 Pro XL: not Samsung-specific). The drain
  that follows leaves the `GetObject` transaction unclosed: it idles out without
  ever seeing the closing Response container, the device then stops answering
  (GET_DEVICE_STATUS times out as `TransferError::Cancelled`, versus the fast
  `Stall` an unsupported device returns), and the session is dead. The wedge is
  intermittent. **Transfer size is not the trigger**: `doctor --probe-cancel`
  reported `wedged_recovered` on a 36-byte file (verified on a Galaxy S23 Ultra
  SM-S918B, macOS/nusb, 2026-07-20). Don't reason about "backlog size" when
  triaging.
  **The signature differs by device**: a Samsung surfaces `Error::DeviceReset` on
  the next op, a Pixel simply **hangs** with no error (verified on a Pixel 9 Pro
  XL, macOS/nusb, 2026-07-20), so consumer detection needs a timeout, not just an
  error match.
  Two ways in, with different outcomes on the same hardware and day:
  - An explicit `cancel()`, or a **dropped windowed** `GetPartialObject64`
    future whose drain runs through `recover_if_needed`, surfaces
    `Error::DeviceReset` and recovers **in software** with spaced retries
    (transport reset, then reopens that return `Timeout`, then
    `SessionAlreadyOpen`, then success).
  - A dropped **held-open streaming** `GetObject` (`FileDownload`) future did
    **not** recover in software: plain reopen and transport reset both failed,
    and it needed a physical replug.
- **How `cancel_transfer` handles it (design C):** when it detects the wedge
  (the `Cancelled` timeout above), it issues a session-less USB `DEVICE_RESET` to
  un-stick the transport and returns `Error::DeviceReset` instead of a false
  success. It does **not** reopen. Reopen is the caller's job and must be
  **quiet**: post-reset the device needs a beat with no USB traffic to finish
  tearing the old session down. Reopen immediately and you get
  `SessionAlreadyOpen`; *hammer* close/open at it (a tight retry loop) and it
  stays busy and re-wedges into a hard `Timeout`. So on `DeviceReset`: drop the
  device, wait a few seconds quiet, then open again (retry with idle-spaced
  backoff, not a tight loop). Paced downloads (the default) shrink what a cancel
  has to abort to one window, but they do **not** remove the wedge: a window
  future dropped mid-flight still wedges the device through the recovery
  drain. It's the recoverable flavor, though, so keep `HeldOpen` off Android.
  See `docs/debugging.md`.
- See `NusbTransport::cancel_transfer()` for the full implementation with
  detailed comments.

## Resumable (offset) streaming downloads

`Storage::download(handle, ByteRange::From(offset))` is a streaming download
that starts at a byte offset and streams `[offset, size)` to EOF. It reuses the
exact `ByteRange::Full` machinery (the `execute_with_receive_stream` →
`ReceiveStream` → `FileDownload` path, SIC class-cancel, multi-transfer data
container accumulation, `TransactionScope`/recovery), just driven by
`GetPartialObject64(handle, offset, max_bytes)` instead of `GetObject`.

The use case is **releasing the one-per-device PTP session**. An in-flight
`GetObject` owns the session until it finishes or aborts, so a host can't list
folders while a download is open (even a "paused" one that parked in place). With
this API a consumer can `cancel()` the in-flight download (drains the pipe via
the validated CLASS_CANCEL path, frees the session so navigation works again),
remember the bytes it kept, and reopen from exactly that offset to fetch the rest:
a true suspend/resume.

Contract:

- `offset == 0` is equivalent to `ByteRange::Full` (whole file), routed through
  `GetPartialObject64`.
- `offset == size` yields an empty stream that ends at a clean EOF (zero chunks).
  Resuming an already-complete file is a no-op, not an error.
- `offset > size` returns `Error::InvalidData` immediately, before any USB I/O,
  so it never hangs waiting for bytes the device can't supply.
- The returned `FileDownload` reports the **full** object `size()` (not the
  segment length), so a resumed download's progress/ETA stays anchored to the
  whole file.
- `GetPartialObject64`'s `max_bytes` is a u32, so a single call requests at most
  `u32::MAX` (~4 GiB) bytes from the offset; a larger tail is fetched across
  multiple resumes. The 64-bit *offset* is what lets a resume start past 4 GB.
- Cancellation mid-partial-stream drains/recovers exactly like the full streaming
  download (see "Transfer cancellation" and "In-session desync self-healing"
  above): a follow-up operation on the same session works.
- Prefers `GetPartialObject64` (`0x95C1`, 64-bit offset), but falls back to the
  32-bit `GetPartialObject` (`0x101B`) when the device advertises only that (many
  PTP cameras, e.g. the Panasonic Lumix DMC-TZ61, issue #12). The fallback covers
  any offset that fits in `u32` (files up to 4 GiB); a resume past 4 GiB still
  needs the 64-bit op, else `Error::InvalidData`. A device with neither op returns
  `Error::Unsupported`. The op choice lives in `plan_partial_read` in
  `mtp::backend::usb` (unit-tested); `Capabilities::supports_partial_download`
  conflates the two ops into one flag, so it can't tell them apart, use it only as
  a coarse "some partial read exists" hint.

Tested against the virtual device in `transport/virtual_device/mod.rs`
(`download_stream_from_offset_*`, `cancel_mid_partial_stream_leaves_session_usable`).
`examples/resume_download.rs` demonstrates the full half-download → pause → list →
resume → verify cycle with no hardware.

## Paced downloads (windows that free the session)

`Storage::download(handle, range)` reads a file as a series of streamed partial reads (windows), each
a bounded `GetPartialObject64` transaction, and frees the one-per-device PTP session between them.
`Storage::download_with_pacing(handle, range, pacing)` picks the `DownloadPacing`:

- `MaxCancelDelay(d)`, the default at `DEFAULT_MAX_CANCEL_DELAY` (2 s): windows sized from the measured
  speed to take about `d`. First window 8 MiB, then smoothed speed (device time only, half weight on
  the newest window) × `d`, rounded down to whole MiB, at least 1 MiB, at most `u32::MAX` (the
  partial read's `max_bytes`).
  **The speed is measured from the moment `download_window` returns**, so it counts only the time
  spent waiting for chunks: the yield before a window and the window's start aren't in it. On WPD
  that start is a stream reopen and seek whenever another call ran in between, and on a forward-only
  stream the seek re-reads the whole prefix, seconds deep into a big file. Counting it read as a slow
  link, shrank the next window toward 1 MiB, let in more calls, and so caused more reopens: a 1 GB
  copy fell to 0.7 MB/s on a POCO X6 Pro under Windows 11 (#61). Keep the start out of the clock. On
  USB it leaves out the window's first round trip (a few ms), so a window runs slightly over `d`.
  **A body that reads ahead reports the device time itself** (`DownloadBody::device_time`), and
  `FileDownload` believes it over its own waits. WPD's worker reads into a bounded channel while the
  consumer handles earlier chunks, so the consumer's waits leave out whatever the device did during
  the consumer's work: with a consumer writing each chunk to disk, 500 ms windows grew to 5.7 s
  (186 MiB) in seven windows (Pixel 9 Pro XL, Windows 10, 2026-11-08). USB reads on demand, so its
  body reports nothing and the waits are right.
- `FixedWindow(bytes)`: windows of that size (0 counts as 1).
- `HeldOpen`: one transfer for the whole range (`GetObject` for `Full`), the old `download`.

The policy lives in `FileDownload` (`mtp/stream.rs`, the `Pacer`); backends only provide
`download` (held-open, size passed in), `download_window` (one streamed partial read, no
`GetObjectInfo`), and `can_read_partially_at(offset)`.

Why (#60, Pixel 9 Pro XL, macOS/nusb, 2026-11-06, `mtp-bench` and a per-window probe):

- A held-open read owns the session for the whole file, and since #54 a cancel on Android drains the
  rest of it, with no cap: a 5 GB file cancelled at 1% over 2 MB/s held the device ~41 minutes.
- **Each window costs ~15–20 ms on top of its data, flat across the file.** A 4 KiB partial read takes
  ~5 ms (round trip plus Android opening the file), a 1 MiB one ~12 ms against ~3.5 ms of stream time,
  so each read also restarts the phone's send pipeline slowly. Cutting host-side copies changed
  nothing, so it's the phone. Hence 8 MiB windows ran at ~139 MB/s and 32 MiB at ~229, against
  268 MB/s held open, and a window sized to take ~2 s costs ~1% at any link speed.

Contract:

- **Windows start lazily** (first `next_chunk`); a held-open read starts at the call, and so does the
  fallback below, so a refusal surfaces from `download`, not the first chunk.
- **Fallback**: where `can_read_partially_at(offset)` is false (no partial-read op, like libhaze; or
  32-bit-only and past 4 GiB), the rest of the range is one held-open read.
- **A window closes in the call that returns its last bytes**: `FileDownload` reads the closing
  response right away, so `holds_session()` is `false` between calls at a window boundary and a
  listing runs at once. A short window (fewer bytes than asked) closes on the next call, which then
  starts the next window from where the bytes stopped. A window that sends more than asked is
  `InvalidData`.
- **The download yields once before each window after the first**, so a call queued during the last
  window gets the lock first. Runtime-agnostic (`YieldNow`), best effort.
- `holds_session()` is for consumers with their own device lock (Cmdr): hold it while `true`, let
  other work in while `false`. Never block the download on a lock while waiting on the device.
- Edge cases: empty file or `offset == size` ⇒ `None`, no read issued; `offset > size` ⇒
  `InvalidData` before any I/O; a 0-byte read while bytes remain ⇒ `InvalidData` (a stall, not EOF);
  a short read advances by what arrived; `Range` past the end stops at the end. `size()` is the whole
  object, `offset()` the next byte (resume from there), `progress()` counts from the start of the
  file. `modified()`/`created()` come from the `ObjectInfo` fetched to start, at no extra cost.
- Stopping: `cancel()` between windows sends nothing; mid-window it cancels that window (on Android:
  drains it, ≤ the pacing target). Dropping is safe: between windows nothing is held; mid-window the
  `ReceiveStream` flags recovery and the next op drains.
- **WPD**: windows go through the worker's `DownloadWindow` request: one MTP partial read through
  the driver's passthrough where it can, else a continuation of the kept read stream like
  `read_range` (§ Windows WPD backend). Either way other requests run between windows; only the
  passthrough makes them free.

Tested against the virtual device in `transport/virtual_device/mod.rs` (the "Paced downloads"
block: reassembly, every pacing, ranges, 32-bit-only, no-partial fallback, short/stall reads via
`force_partial_read_caps`, the session free between windows, a queued listing getting in, cancel and
drop mid-window), plus `Pacer` unit tests in `mtp/stream.rs`. The virtual device's bulk reads
return at most the 64 KiB asked for, like USB (#63), so a window over 64 KiB arrives in several
chunks and is mid-window after the first. A smaller window (the tests use 512–4,000 bytes) arrives as
one chunk, so a test there needs a forced short read to be mid-window.
`examples/paced_download.rs` demonstrates it with no hardware. Real-device coverage:
`test_paced_download_matches_held_open_and_frees_session` in `tests/integration.rs` (`#[ignore]`),
and `mtp-bench --operations stream,paced,windowed`.

## Stall recovery and device reset

- Devices STALL a bulk endpoint to signal errors (cameras do this for
  unsupported operations); the halt persists until cleared, even across
  process restarts. Every bulk completion site clears the halt via
  `clear_halt` on STALL.
- **Blocking nusb ops (`clear_halt`, `claim_interface`, `set_configuration`,
  `open`, `list_devices`) go through the `blocking()` helper in
  `transport/nusb.rs`, never `.await`.** nusb wraps them in `MaybeFuture`s
  backed by a blocking syscall; awaiting one panics at runtime ("Awaiting
  blocking syscall without an async runtime") unless the consumer enables
  nusb's `tokio`/`smol` feature, which we don't (runtime-agnostic). The trap is
  that `impl MaybeFuture` also implements `IntoFuture`, so `.await` compiles and
  only blows up on real hardware — a STALL never fires against the mock/virtual
  transport, so a stray `.await` slips through CI (#12 hit it on the first
  camera stall, and it's acknowledged upstream in kevinmehall/nusb#212). The
  `blocking()` choke point removes the per-call-site `.wait()`-vs-`.await`
  choice and holds the rationale in one place; new blocking calls should use it.
  `control_in`/`control_out` are the exception: genuinely async via nusb's URB
  event loop, so those stay `.await` and must NOT go through `blocking()`.
- `Transport::reset_device()` / `PtpDevice::reset_device()` /
  `MtpDevice::reset_by_serial()` (plus `reset_by_location` / `reset_first`, and
  the `MtpDeviceBuilder` forms that honor `timeout` and `known_devices`) / the
  CLI's `mtp-rs reset` send the SIC DEVICE_RESET request (0x66), clear halts, and
  drain stale bulk data (without a PTP session), so they work on a device too
  wedged for `OpenSession` ("Transaction ID mismatch" / "expected Response
  container type" on every command).
- **The reset is a LAST resort on Android, not step two.** It can break the
  phone's MTP function until the user physically replugs: Android's `MtpServer`
  loses its endpoint and never re-arms, while USB keeps enumerating (verified on a
  healthy Pixel 9 Pro XL, macOS/nusb + `adb logcat`, 2026-07-21). Quiet spaced
  reopens come first, and they're enough for a Pixel's dropped-future wedge. Don't
  reword the reset docs back into "the recovery recipe".
- **But keep the automatic reset in `cancel_transfer`**: it only fires after a
  detected wedge. Control run (macOS/nusb, 2026-10-30): a Pixel 9 Pro XL's cancel
  wedge is `MtpServer` exiting on a lost `io_cancel` race (~1 cancel in 5 on a 697 KB
  file), dead with or without the reset, so the reset costs nothing there. A Galaxy S23
  Ultra recovered 4 of 4 with it and 0 of 2 without. Evidence:
  [docs/notes/android-wedges-and-the-reset-kill-switch.md](docs/notes/android-wedges-and-the-reset-kill-switch.md).
  On Android it's now a backstop: there's no CLASS_CANCEL to lose a race (§ Transfer cancellation).
- **The neutral reset is a free-standing selector, not a method on an open
  `MtpDevice`, and that's deliberate.** It claims the USB interface and stops;
  the regular opens run `OpenSession` + `GetDeviceInfo`, which a wedged device
  can't answer, so a method on an open device would be useless exactly when it's
  needed. Callers must drop the device first (holding it keeps the interface
  claimed, so the reset couldn't claim it) and reopen after, since the session is
  gone either way. Virtual devices return `Error::Unsupported`: no USB transport
  to reset.

## In-session desync self-healing (abandoned transactions)

A PTP transaction is command → (data) → response over one bulk pipe, and the
host's transaction-ID counter must track the device's. If an operation's future
is **dropped** after its command goes out but before the response is drained (a
superseded listing, a cancelled task, a `timeout` racing the future), or it
returns early on an I/O error, the device's reply is left in the pipe. Every
later operation then reads the *previous* reply, so the IDs desync by one
("Transaction ID mismatch: expected N, got N-1") and stay desynced: the session
is dead until reset. This is the in-session cousin of the cross-process poison
`reset_device()` fixes, and it's how a dropped Android/Xiaomi listing made a live
device look disconnected.

`PtpSession` heals this automatically and transparently:

- Every operation (`execute`, `execute_with_receive`, `execute_with_send`, the
  streaming send, and a `ReceiveStream`) is **armed** across its command→response
  cycle via a `TransactionScope` guard. A clean completion disarms; any other
  exit (drop, `?`, ID mismatch) flags shared `RecoveryState` with the in-flight
  transaction ID.
- The next operation, under the `operation_lock`, calls `recover_if_needed()`
  first: if flagged, it drains the bulk pipe via `cancel_transfer` (the validated
  CLASS_CANCEL + read-until-idle path) before sending, realigning the stream.
- Recovery is **lazy** (next-op, not in `Drop`) because `Drop` can't run an async
  drain in a runtime-agnostic crate. `ReceiveStream::drop` flags recovery when
  abandoned mid-stream; `cancel()` is still preferred (drains promptly, vs. on
  the next op). Consumers need no code changes.
- **A late reply to an earlier transaction is skipped, not fatal** (`is_stale_reply`
  in `ptp/session/mod.rs`, the libgphoto2 "suspecting old reply" rule). When a
  response carries an ID *behind* the current one, before any data arrived, the
  operation drops it and reads again, at most `MAX_STALE_REPLIES` times. Seen on a
  POCO X6 Pro (#43): after a refused CLASS_CANCEL, the cancelled download's
  closing reply arrived after the drain had stopped waiting, and the next listing
  read "expected 119, got 118"; every later command then read its predecessor's
  leftovers, across processes too. A reply with a *later* ID is still a desync
  error, and `CloseSession` never skips (a Leica answers it with ID 0).
- **A dead host process's leftover transfer is drained at open.** Recovery state lives in
  the process, so when a process dies mid-download, the next one starts unflagged while the
  phone still sends the rest of the object. If the `OpenSession` reply doesn't parse as a
  response, `send_open_session` calls `Transport::drain_bulk_in` (64 KiB reads until 300 ms
  idle, capped at 30 s; it sends nothing to the device) and asks once more. The drain also
  takes the dead transaction's reply and the reply to the first `OpenSession`, so the retry
  usually gets `SessionAlreadyOpen` and the close-and-reopen path finishes the job. Only a
  parse failure triggers it, so a healthy open costs nothing. Before this, each new process
  read one 512-byte chunk of the object as its reply and failed, until a replug (POCO X7 Pro
  on Linux, a process killed 50 ms into a 4 MB window, #43). Don't answer this with the SIC
  device reset: on a Pixel it kills MTP until a replug.
- Tested against the mock with a controllable mid-receive suspension
  (`MockTransport::block_receive`) in `ptp/session/mod.rs`
  (`abandoned_receive_*`). The mock's `cancel_transfer` drains its queued
  responses to mirror a real pipe drain.

## Streaming uploads (USB bulk transfer details)

Uploads use `Transport::send_bulk_streaming()` to avoid buffering the entire
file in RAM. Key implementation notes:

- **`SendObjectInfo` and `SendObject` go out under one hold of the session's
  operation lock** (`PtpSession::send_object_with_info`, built on the `*_locked`
  variants of `send_object_info` and `execute_with_send*`). AOSP's
  `MtpServer::handleRequest` forgets a pending `SendObjectInfo` on any other
  command, and `doSendObject` then answers `NoValidObjectInfo` (0x2015) without
  reading the data phase. On a Pixel 9 Pro XL that refusal wedged MTP until a
  replug: our bulk OUT waited for a device that was waiting to send its answer
  (macOS/nusb, 2026-11-11, one run, `docs/specs/paced-uploads.md`). Two separate
  calls left a gap a few instructions wide, but a listing from another task got
  in about once per 1,000–3,000 uploads on the virtual device
  (`upload_survives_listings_from_another_task`). Don't split the two again.
  Low-level `ptp::` callers using `send_object_info` + `send_object_stream` still
  have the gap; the `send_object_info` docs say so. The virtual device models the
  AOSP behavior with `android: true`.
- **A `SendObject` refused before its data phase still costs the full timeout.**
  Noticing the early answer would mean reading bulk IN while the bulk OUT is
  pending, then cancelling an OUT transfer the device never reads; what the
  phone makes of the bytes that already went out (AOSP reads them as commands
  and drops them) hasn't been tried on hardware, so it isn't done. The lock above
  removes the trigger we know of.
- PTP data containers can span multiple USB bulk transfers. The device
  detects end-of-data via a short packet (< max packet size) or a
  zero-length packet (ZLP) when the total is a multiple of max packet size.
- Each `Endpoint::submit()` call is a separate USB transfer. The header
  (12 bytes) is prepended to the first chunk so the device sees the PTP
  container header in the first transfer (matching libmtp behavior).
- Data is batched into 256KB USB transfers using nusb's low-level
  `allocate/submit/next_complete` API. `EndpointWrite` would be
  cleaner but requires ownership of the `Endpoint`, which lives behind
  a `Mutex` in `NusbTransport`.
- **Every bulk OUT wait is async** (`complete_out` in `transport/nusb.rs`:
  `next_complete` raced against a `futures_timer::Delay`, cancel-and-collect
  on timeout). Don't go back to `wait_next_complete`/`transfer_blocking`:
  they block the executor thread for the whole upload, so a consumer's
  timeout or `select` couldn't stop an upload mid-transfer. Because the wait
  is async, a dropped upload can leave one transfer in flight; `settle_out`
  collects it before the next send, so nothing queues behind it unseen.
- A ZLP must be sent after the final transfer if its size is a multiple
  of `max_packet_size`. Without this, Android devices hang waiting for
  more data (validated on Pixel 9 Pro XL).
- Mock and virtual transports use the default implementation which
  buffers everything and calls `send_bulk()`.
- See `NusbTransport::send_bulk_streaming()` for the full implementation.

### Partial-handle contract on upload failure

`Storage::upload` / `upload_with_progress` are two-phase: `SendObjectInfo`
creates the object on the device (returning a handle), then `SendObject` streams
the bytes. If the data phase fails or is cancelled, the device keeps a partial
(empty or truncated) object. Both functions return `Result<ObjectHandle,
UploadError>`; on a data-phase failure `UploadError::partial` is `Some(handle)`
so the caller can `delete` it or retry the data phase to resume. The library
**never** auto-deletes it: that would issue hidden USB I/O to a possibly-gone
device, the leave-vs-delete behavior is device-dependent, and PTP's design
intends a failed `SendObject` to be retriable against the same handle. We push
the cleanup-or-resume policy to the consumer. `From<UploadError> for Error` keeps
`?` ergonomic; callers drop `partial` unless they match on `UploadError`. The
virtual device creates the object (empty placeholder) at `SendObjectInfo` time,
like real devices, so the handle is real before the data phase.

**Android removes the object itself when an upload stops partway, so `partial` is
`None` there.** After a source failure (a progress `Break` arrives as one),
`UsbBackend::upload` asks `GetObjectInfo` once: `InvalidObjectHandle` means the
device dropped it, any other answer keeps `Some`. See the next section for why
the device drops it.

### Ending an upload that stopped partway (#62)

A `SendObject` that stops mid-data (a `ControlFlow::Break`, a source error, or a
dropped future) leaves Android's `MtpServer` inside `MtpFfsHandle::receiveFile`,
reading bulk OUT as file data for as long as it takes. Before the fix, the next
command went in as file data: a 12-byte short packet, so the server logged
"unexpected short packet", unlinked the file, and answered `GeneralError` for the
*`SendObject`*. The host skipped that as a stale reply and waited for an answer
to its own command that never came: every first operation after a stopped
upload timed out (30 s by default), and the one after it worked. Deterministic,
not a race: 5 of 5 runs (Pixel 9 Pro XL, macOS/nusb, 2026-11-08).

The fix: `TransactionScope::set_sending` marks the data-out phase open, and
recovery calls `Transport::end_send_phase` before its drain. On Android
(`set_drain_on_cancel`) that sends a zero-length packet, which the server reads
as the short packet above, so it answers the `SendObject` and the drain takes
the answer. The data sent before always ends on a whole `max_packet_size`, so
the ZLP is the first short packet. A source failure (`Io`) runs this at once,
inside `execute_with_send_stream`, so the upload returns with the session clean;
a dropped future and transport errors leave it to the next operation. Off
Android `end_send_phase` does nothing: `cancel_transfer`'s CLASS_CANCEL is PTP's
way to end a data phase, and no camera has been tried, so don't widen the ZLP
without one. **Hardware-verified on a Pixel 9 Pro XL** (macOS/nusb, 2026-11-08,
`test_upload_stopped_midway_then_session_works`, 268 MB file, stopped at 10% and
50%): 20 of 20 `Break` runs returned 0.05–0.44 s after the break and the next
listing took ~0.1 s; 20 of 20 dropped futures, next listing 0.14–0.29 s; four
500 MB runs the same. No object left behind in any run, no wedge, no replug. The
virtual device models the same AOSP behavior (`end_data_phase_short`), and
advertises `android.com` so the host treats it as Android.

## Receiving data containers (multi-transfer convention)

PTP data containers may span multiple USB bulk transfers on receive too: some
devices (Garmin Forerunner 955, observed) send the 12-byte container header in
one bulk transfer and the payload in a follow-up transfer. **Any new code path
that calls `receive_bulk()` and expects a `DataContainer` must accumulate
transfers until `bytes.len() >= total_length` (read from the first 4 bytes of
the header) before parsing.** See `PtpSession::execute_with_receive` and
`PtpDevice::get_device_info` for the canonical pattern. Skipping this loop
breaks GetDeviceInfo on spec-compliant devices that split.

`ReceiveStream` (the streaming download path) does **not** accumulate: it hands
each chunk out of a `BytesMut` with `split_to`, which advances the front, so the
buffer holds about one 64 KiB read no matter how big the object is. Peak memory
for a streaming download is that buffer plus whatever the consumer keeps, so a
4 GB file costs the same as a 4 MB one. **Don't reintroduce an accumulate-then-
slice shape here**: the old one held the whole object, because a PTP data
container for an object *is* the whole object.

Objects over 4 GiB don't fit a 32-bit `ContainerLength`, so responders send the
`0xFFFFFFFF` sentinel instead (MTP 1.1 appendix H.1) and the real byte count
comes from the `ObjectSize` object property. `ReceiveStream` handles both ends of
that: `execute_with_receive_stream_sized` takes the resolved size and stops on
the byte count, and without it the data phase ends at the first short packet
(the USB signal), with a zero-length packet tolerated as the terminator when the
payload exactly fills a read. `UsbBackend::download` passes the size for
`ByteRange::Full` only when `get_object_info_full` actually resolved it past
`u32::MAX`: a size still saturated at `u32::MAX` is unknown, not 4 GiB, and
passing it would truncate the download. Real-device coverage:
`test_big_file_over_4gib_round_trip` in `tests/integration.rs`, double-gated
behind `#[ignore]` and `MTP_TEST_BIG_FILE=1` so a plain `--ignored` sweep never
writes 4+ GB to someone's phone.

## Test-time backing-dir drain (virtual-device only)

External test fixtures that delete and recreate files in a virtual device's backing dir hit the same race the watcher's pause/resume is designed to prevent: FS events from the writes can land *after* the rescan and resume, and the watcher then incorrectly emits removes for the freshly re-added objects. Old approach: pause, write, sleep ≥600 ms (macOS FSEvents worst-case), rescan, resume. Slow and brittle.

The current API supports an event-driven drain:

- `pause_watcher(serial)` returns a `WatcherGuard`. The pause is **refcounted** (`VirtualDeviceState::pause_count`), so multiple concurrent guards compose: the watcher only resumes when the last guard drops. Tests can drain in parallel without stepping on each other.
- While at least one guard is alive, every dropped FS event's canonical path is pushed into `VirtualDeviceState::dropped_paths` (a `VecDeque` capped at `DROPPED_PATHS_CAP = 1024`, oldest evicted past that).
- `dropped_paths_since_pause(serial) -> Vec<PathBuf>` is the **primary observation primitive**: returns a clone of the ring, oldest first.
- `was_path_dropped(serial, suffix) -> bool` is a thin convenience over the above for the sentinel-file drain pattern: write a uniquely-named file as the LAST fixture step (per-directory FS-event ordering on every supported `notify` backend means every earlier write to the same directory already arrived once you see the sentinel), then poll this until it returns `true`.
- `clear_dropped_paths(serial)` empties the ring; call after a successful drain so the buffer stays scoped to in-flight pauses.

**Why suffix-match, not exact path**: macOS canonicalizes `/tmp` → `/private/tmp`, the watcher canonicalizes again, and the backing-dir path may be relative. Suffix-match sidesteps the whole class of false negatives. Choose a unique enough suffix (UUID-bearing filename) so concurrent drains don't false-positive on each other.

**Composing your own pattern**: any test harness that doesn't fit sentinel-file (counting events under a subdir, declaring quiet when the count hasn't grown for N polls) should call `dropped_paths_since_pause` directly. `was_path_dropped` exists for the common case only.

Unit tests for the API live in `transport/virtual_device/registry.rs` (`pause_refcount_composes_across_concurrent_guards`, `dropped_paths_observation_round_trip`, `dropped_paths_ring_evicts_oldest_past_cap`, and the unknown-serial defensive paths).

## Simulating stale handles (virtual-device only)

`rekey_virtual_object(serial, rel_path)` reassigns a tracked object's handle while leaving the object and its on-disk contents in place: the old handle then returns `InvalidObjectHandle`/`InvalidParentObject`, a fresh listing of the parent surfaces the new handle, and direct children are re-parented. It queues no events (it models the device moving on before the host observes the change), so it reproduces the Android handle re-keying quirk above: the exact precondition for a stale-cached-handle upload/delete failure. The stable-handle virtual device can't otherwise produce it. Drive it with a list → `rekey_virtual_object` → operate sequence to exercise a host's stale-handle recovery path; see `rekey_object_invalidates_old_handle_then_relist_and_upload_recover` in `transport/virtual_device/mod.rs`.

## Windows WPD backend (`cfg(windows)`)

The `mtp::backend::wpd` backend implements `MtpBackend` over the Windows Portable Devices COM API
(`windows` crate, `cfg(windows)`), as a sibling to `UsbBackend` — *not* a `Transport`, because WPD
speaks MTP itself and blocks the raw opcodes. Quirks and
semantics that differ from the USB/PTP backend, all hardware-verified on a Pixel 9 Pro XL:

- **Threading**: one dedicated `std::thread` per open device owns *all* COM pointers (they're
  apartment-affine / `!Send`), `CoInitializeEx(MTA)`, and serves one request at a time off a channel.
  `WpdBackend` holds only channel senders → `Send + Sync` with zero `unsafe` in the public path. A
  streaming download reads `IStream` chunks into a bounded channel; dropping/cancelling the receiver
  fails the worker's next send, which stops the read and releases the `IStream` — WPD "cancel" is
  just stop-reading + `Release` (no SIC class-cancel).
- **Streaming upload, transactional commit (partial-handle differs from USB)**: upload is
  `CreateObjectWithPropertiesAndData` → write chunks → `Commit`. The source is **streamed** straight to
  the worker over a bounded channel (`DATA_BOUND`), written chunk-by-chunk as it arrives — nothing
  buffers the whole file (peak memory ≈ a few in-flight chunks, ~1 MiB; verified ~13 MiB peak working
  set streaming a 300 MiB upload). The consumer drives the source, reports progress, and honors a
  `ControlFlow::Break` by closing the channel early. The object is created *before* all bytes arrive,
  so on a short close the worker releases the stream **without** `Commit` and probes the parent for any
  leftover partial. **Hardware finding (Pixel 9 Pro XL)**: `Release`-without-`Commit` discards the
  object entirely — both a cancel-before-any-data *and* a cancel after several MiB were written leave
  **no** partial object. So `UploadError::partial` is `None` on WPD (unlike the USB two-phase
  `SendObjectInfo`/`SendObject`, where the data phase can leave a partial the caller must clean up).
  The probe stays in the path defensively: a device that *did* keep a partial would surface its handle.
- **Forward-only resource streams**: `IStream::Seek` returns `E_NOTIMPL` on the Pixel. `stream_seek`
  falls back to read-and-discard, so ranged/resumed `download` (`ByteRange::From`/`Range`) and
  `read_range` stay correct (at the cost of reading the skipped prefix). Verified-seekable streams
  take the fast path. **Caveat**: because the skipped prefix is re-read, a resume from offset is
  O(offset) on such devices, and the usual "release the session and resume later" win shrinks — the
  device re-streams every byte before the offset on each resume. Resuming near the *end* of a large
  file re-reads almost the whole file. Prefer a single in-order pass (`ByteRange::Full` or windowed
  download) over many small offset resumes when the device's `Seek` is `E_NOTIMPL`.
- **Windows and `read_range` are real MTP partial reads where the driver allows** (`passthrough.rs`,
  research and probe numbers in `docs/specs/wpd-partial-reads.md`). WpdMtpDr runs raw MTP
  operations through `SendCommand` (`WPD_COMMAND_MTP_EXT_*`), so a window is one
  `GetPartialObject64` (0x95C1), or `GetPartialObject` (0x101B) for offsets that fit 32 bits on
  devices without the 64-bit op: execute, `READ_DATA` until the data is in, `END_DATA_TRANSFER`.
  No stream is kept, so a call between windows costs nothing.
  - **When**: the driver lists the three `MTP_EXT` commands (checked at open), the device's
    `DeviceInfo` has the op (one passthrough `GetDeviceInfo` on the first read, then cached), and the
    WPD object id is `o<hex>`, an undocumented WpdMtpDr detail. The parsed handle is checked once
    per object with a passthrough `GetObjectInfo` against WPD's name and size, since a wrong mapping
    would read another file's bytes. Anything else, and `read_range` with no length, takes the kept
    stream below. `can_read_partially_at` stays `true` at any offset either way: the stream path
    serves any offset too.
  - **Refusals**: a failed start (execute error, or no data and a non-OK response) sends that read to
    the stream. `Unsupported` or `AccessDenied` also turn the passthrough off for the session;
    `Disconnected` propagates.
  - **Never end a data phase early.** A consumer that stops mid-window (a cancel, a dropped
    download) gets no more chunks, but the worker reads the rest of the window and drops it, then
    ends normally. The driver's early `END_DATA_TRANSFER` is its own cancel (the probe saw
    `ERROR_CANCELLED` and a healthy session after one), and on Android that may hit the `io_cancel`
    race #54 removed `CLASS_CANCEL` for. Costs the rest of one window. A loop of early ENDs with no
    wedge is what it would take to change this.
  - **Never send `OpenSession`/`CloseSession` or a write through it**: the driver owns the session and
    caches object properties.
  - **Hardware** (Pixel 9 Pro XL, Windows 10 22H2, 2026-11-08, `examples/paced_with_calls.rs`, 1 GiB
    file, `MaxCancelDelay(500 ms)`): held open 28.9 s; paced with no calls 31.8 s; a folder listing, a
    4 MB photo, and a thumbnail between every 8th window 33.8 s, against 138 s before (each window
    after the calls re-read the prefix: 4.6 s at 128 MiB up to 27.5 s at 896 MiB); the same between
    every window 60–70 s, of which the calls themselves took 20–35 s, against a download that fell to
    2 MB/s and then failed with `ERROR_SEM_TIMEOUT` (0x80070079) before. Every result's SHA-256
    matched. 20 cancels mid-window, each followed by a listing: no wedge, the listing waited
    ~370 ms (the rest of the 8 MiB window, then the listing).
  - **After that `ERROR_SEM_TIMEOUT`, the Pixel served no data** (`0x80070141`, device not available)
    while listings still answered, until `pnputil /restart-device` on its USB composite device. Seen
    twice, both on the old stream path. Worth knowing when a Windows report says reads fail but
    browsing works.
- **Otherwise, windowed reads continue on one stream**: the worker keeps the stream of the last `read_range` or
  `DownloadWindow` that stopped before EOF, with its position, and either request for the same object
  starting exactly there reads on from it. `DownloadWindow` (a window of a paced download) streams its
  chunks over a channel and keeps the stream only when it sent the whole window; EOF, an error, or a
  consumer that stopped reading releases it. A held-open `Download` keeps the worker for the whole
  transfer, which is why paced downloads use windows here too. Without this every window reopened the stream and, on forward-only devices,
  re-read the whole prefix: a 194 MB video on a Xiaomi POCO X7 Pro (Windows 11) fell below 1 MB/s by
  128 MB. **An open stream keeps the device busy** (any other request fails with "device busy"), so
  the worker releases it before serving any other request, and after `IDLE_READ_STREAM_RELEASE` (1 s)
  with no request, so a paused copy doesn't hold the phone. A window that follows other work or a
  pause reopens the stream and pays the prefix once. The worker drops the kept stream before
  `CoUninitialize`, also when the request channel closes without a `Shutdown`.
- **Any call can stall for minutes after changes on the phone** (#52): 301–377 s on a POCO X7 Pro under
  Windows 11, in a long-lived app, then normal service. `list`, a stale-handle `object_info`, and `delete` all
  did it, and File Explorer stalled the same way with no mtp-rs running (3–4 min), so nothing in this backend
  causes it. Linux never stalled on the same phone. Likely the same wait as the slow open after a first write
  (#43, ~37 s–5 min, five phones). No fix here, but apps can wrap calls in a timeout and report it: dropping the
  future is safe (the worker ignores the unread reply), though later calls queue behind the stuck one and a
  `delete` can still finish after the app gave up. **Phone logcat can't tell you the driver sent nothing**:
  AOSP logs commands with `ALOGV` and responses only when they aren't OK.
- **Diagnostics** (the `tracing` feature, `debug` level, targets under `mtp_rs::mtp::backend::wpd`): the worker
  logs every request served (`served List in 412ms (queued 3.1s)`; for `Download`/`DownloadWindow`/`Upload`
  that's the whole transfer), every release of the kept read stream with its object, offset, reason (`before
  <request>`, `not continued by <request>`, `idle for 1 s`, `worker shutdown`) and how long the `Release`
  took, and for `Download`/`DownloadWindow`/`ReadRange` whether the kept stream continued or a new one was
  opened, the `GetStream` time, the positioning (`Seek` worked, or failed with its HRESULT and the prefix was
  read and discarded, with bytes and time), and the bytes and time streamed (a window's time includes waiting
  for the consumer). `Thumbnail` logs its open and read times. Timing goes through `trace::DiagTimer`,
  zero-sized without the feature. Added for #61 (a paced download on a POCO X6 Pro under Windows 11 spent
  ~8 s per window once other calls ran between windows). To capture: enable `mtp-rs/tracing`, install a
  subscriber with the filter `mtp_rs::mtp::backend::wpd=debug` (`tracing-subscriber` with `env-filter`:
  `tracing_subscriber::fmt().with_env_filter(EnvFilter::new("mtp_rs::mtp::backend::wpd=debug")).init()`). The
  CLI has both built in: `RUST_LOG=mtp_rs::mtp::backend::wpd=debug mtp-rs get …` (`--trace` gives all of
  `mtp_rs=debug`), though one CLI command can't put other calls between windows.
- **`object_info` is strict; listing is lenient**: a single `object_info` lookup must *fail* for a
  deleted/missing object (it errors `NotFound` when no property resolves), whereas the per-child
  listing reader is lenient (an unreadable child degrades to a default record rather than failing the
  whole listing — mirroring the Lumix datetime leniency).
- **Opaque handles**: `ObjectHandle`/`StorageId` tokens are a *deterministic* hash of the WPD
  object-id string (`ids.rs`), so a `StorageId` the CLI prints stays valid across its
  one-process-per-command invocations. A `bimap` resolves tokens back to WPD id strings; a deleted
  object keeps its bimap entry (so its handle still resolves to a string) but resolves no properties.
- **Capabilities** are derived from `IPortableDeviceCapabilities::GetSupportedCommands` (sensible MTP
  defaults if the probe yields nothing). `supports_thumbnails` is `true`: `thumbnail()` reads the
  `WPD_RESOURCE_THUMBNAIL` resource on the worker (verified non-empty for a real JPEG on the Pixel);
  whether a *given* object has one is resolved at call time (objects without a thumbnail fail at
  `GetStream` → `Unsupported`/`NotFound`).
- **Events**: an `IPortableDeviceEventCallback` (`events.rs`), registered with `Advise` on the worker
  thread that owns the device, feeds a channel that `next_event` reads. It waits indefinitely, and a
  closed channel means the session ended (`Error::Disconnected`).
- **Selection**: on Windows, `open_first`/`open_by_serial` default to WPD (`Backend::Auto`), falling
  back to USB when no WPD device is present; `Backend::Usb` forces PTP-over-USB (e.g. a Zadig-bound
  camera), `Backend::Wpd` forces WPD.
- **Toolchain**: build on Windows with the MSVC toolchain. The GNU one (`x86_64-pc-windows-gnu`)
  breaks on a non-ASCII user profile path (MinGW's ANSI `ld` failed on `C:\Users\Felhasználó`);
  MSVC's `link.exe` is Unicode-safe. `raw-dylib` means no Windows SDK import libs are needed either
  way.

## Things to avoid

- C dependencies (libusb, libmtp, `-sys` crates)
- Device quirks database (understand issues first)
- MTPZ, vendor extensions, playlist/metadata sync
- Legacy workarounds (pre-Android 5.0)
- Runtime dependencies (use `futures` traits)

## Code style

Run `just check` before committing. `cargo fmt`, `cargo clippy -D warnings`, tests for new functionality, doc comments for public APIs.

## References

- [docs/architecture.md](docs/architecture.md), [docs/protocol.md](docs/protocol.md)
- [docs/debugging.md](docs/debugging.md): debugging hub. Real-device setup and recovery (ptpcamerad blocker, software reset, fast-fail timeouts, device gotchas) plus USB capture. Read before touching physical hardware.
- [docs/releasing.md](docs/releasing.md): how to publish a new version to crates.io, and how `mtp-rs-cli-v*` tags build prebuilt CLI binaries and the Homebrew formula via dist
- [docs/specs/paced-uploads.md](docs/specs/paced-uploads.md): research spike for #62, uploads in windows through Android's edit ops (0x95C2–0x95C5). Wire shapes from AOSP, hardware findings so far, and the probe (`examples/android_edit_probe.rs`). Read before touching uploads or the edit ops.
- [SECURITY.md](SECURITY.md): how to report a vulnerability, and that only the latest release gets fixes.
- [docs/ptp-camera-features.md](docs/ptp-camera-features.md): camera control through `ptp::` (properties, capture, events), using only stable API.
- [docs/dependency-updates.md](docs/dependency-updates.md): playbook for Renovate PRs. How the config groups and automerges, how to read a red PR, crates that must move together, per-dependency holds (`nusb`, `windows`), the MSRV policy (why nothing is ever capped for the MSRV, and how CI checks it), and everything an MSRV raise touches. Read before merging or closing a dependency PR.
- [docs/notes/android-wedges-and-the-reset-kill-switch.md](docs/notes/android-wedges-and-the-reset-kill-switch.md): what a day of hardware work established about wedged Android MTP sessions. The two wedge signatures (Samsung errors, Pixel hangs), the dropped-future trigger, why the transport reset kills a Pixel's MTP function, what recovers what, and which claims rest on a single observation. Read before changing reset or recovery guidance.
- [docs/notes/community-threads.md](docs/notes/community-threads.md): required reading before working on issues or PRs. Recap of every GitHub thread so far, known device quirks, and recurring contributors. Update after work that affects community-facing context.
- [MTP v1.1 Spec](https://github.com/vdavid/mtp-v1_1-spec-md)
