# mtp-rs

Pure-Rust MTP/PTP library with no C dependencies. Two-layer API: `mtp::` for high-level file transfer, `ptp::` for low-level protocol access (cameras). Zero FFI - no libmtp, no libusb, just async Rust on `nusb`.

This repo is a Cargo workspace. The library lives in `crates/mtp-rs/` and is published as `mtp-rs`. A companion CLI binary lives in `crates/mtp-rs-cli/` and is published as `mtp-rs-cli` (the installed binary is named `mtp-rs`).

## Quick commands

- `just`: Run all checks: format, lint, test, doc
- `just fix`: Auto-fix formatting and clippy warnings
- `just check-all`: Include MSRV check, security audit, license check
- `just release-dry`: `cargo publish --dry-run` for both crates
- `cargo test --workspace --all-features`: Run with proptest fuzzing across the workspace

## Project structure

```
crates/
  mtp-rs/                    # Library (crates.io: mtp-rs)
    src/
      mtp/                   # High-level API (MtpDevice, Storage)
      ptp/                   # Low-level protocol (PtpDevice, PtpSession)
        codes.rs             # OperationCode, ResponseCode, EventCode
      transport/             # USB abstraction (Transport trait, nusb, mock, virtual_device)
    tests/integration.rs     # Real-device tests
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
  +-- WpdBackend (planned, cfg(windows)): WPD over COM
ptp:: (PtpSession)            <-- Cameras, protocol work (USB-only by nature)
  |
transport:: (Transport trait)
  |
nusb (USB)  or  VirtualTransport (filesystem, feature = "virtual-device")
```

The `mtp::` layer is **backend-neutral**: it speaks neutral types (`mtp::{ObjectHandle, StorageId,
ObjectInfo, ObjectFormat, DeviceInfo, StorageInfo, Capabilities, DateTime}`) and the neutral
`mtp::Error`, and dispatches through the `MtpBackend` trait. `MtpDevice`/`Storage` are thin façades
over `Box<dyn MtpBackend>`; `UsbBackend` is the sole implementation today and holds the `PtpSession`,
converting PTP↔neutral only at its boundary (via `to_ptp`/`from_ptp` on the neutral types and
`From<PtpError> for mtp::Error`). All device-quirk logic (root-listing fast path, Android/Samsung/Fuji
fallbacks, >4 GB size resolution, SIC cancel, recovery, the upload partial-handle contract) lives in
`UsbBackend`. The virtual device and mock are **not** separate backends — they're `Transport`s under
`UsbBackend`, so every existing test exercises the real backend path. A Windows WPD backend is the
planned second `MtpBackend` (see `docs/windows-wpd-backend-plan.md`).

**Errors:** `mtp::Error` (re-exported as the crate-root `Error`) is the neutral high-level error with
backend-agnostic variants (`NotFound`, `StaleHandle`, `AccessDenied`, `Unsupported`, `Busy`,
`StorageFull`, `Cancelled`, `Disconnected`, …). The rich low-level PTP error is `PtpError` (root and
`ptp::PtpError`); `ptp::` keeps its detailed response-code errors for camera/protocol users.

**Entry points:** `MtpDevice::open_first()`, `PtpDevice::open_first()`, `NusbTransport::list_mtp_devices()`, `mtp::watch_devices()` (hotplug stream), `MtpDevice::reset_by_serial()` (session-less transport reset),
`MtpDeviceBuilder::open_virtual()` (feature-gated)

**Key types:** `ObjectHandle`, `StorageId` (opaque `u64` newtypes, session-scoped tokens — not wire
values), `ObjectFormat` (raw MTP format code + category helpers), `Capabilities` (replaces the old
`is_android`/`supports_*` accessors; `MtpDevice::capabilities()`, plus convenience
`supports_rename()`/`supports_upload()`), `ByteRange { Full, From(off), Range { offset, len } }`
(drives `download`/`download_windowed`), `UsbSpeed` (negotiated USB link speed on
`MtpDeviceInfo::speed` / `UsbDeviceInfo::speed`; both info structs are `#[non_exhaustive]`).

**Download API:** three patterns over `ByteRange` — streaming `Storage::download(handle, range)`
(holds the session, returns `FileDownload`), session-releasing `Storage::download_windowed(handle,
range, window_size)` (returns `WindowedDownload`), and buffered `Storage::download_to_vec(handle)` /
`Storage::read_range(handle, offset, len)` / `Storage::thumbnail(handle)`.

## Known device quirks

- **Android**: `ObjectHandle::ALL` recursive listing broken; library auto-detects via `"android.com"` in vendor extension
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
- **Fujifilm cameras**: Report `AccessCapability::ReadWrite` but return `StoreReadOnly` on writes. Advertised ops lie.
- **Samsung**: Returns `InvalidObjectHandle` for root listing; needs recursive traversal with filtering
- **Panasonic Lumix DMC-TZ61** (and likely other PTP cameras): Reports `20480000T000000` (month 0, day 0) as "no date" in ObjectInfo datetimes. Receive-side datetime parsing is lenient for this reason: unparseable datetimes become `None` instead of failing the dataset parse. Send-side packing stays strict.

## Testing

> **Debugging against real hardware? Read [docs/debugging.md](docs/debugging.md) first.**
> It's the debugging hub: how to keep macOS's `ptpcamerad` off the interface (run
> the blocker yourself), recover a wedged device (quiet spaced reopens first,
> `mtp-rs reset` last: on Android the reset can break MTP until a replug), fail
> fast with `MTP_TEST_TIMEOUT_SECS=2`, and the Android gotchas. Only needed when a
> physical device is involved.

- **Unit**: `cargo test --workspace` (uses mock transport)
- **Filesystem-watcher tests run in their own pass** (`just test` does this for you: everything else, then `fs_watcher` with `--test-threads=1`). They wait on real OS filesystem-event delivery, so inside the ~400-test parallel pool a loaded machine starves them past their poll budget and they fail as a group while passing every time alone. Don't fold them back in, and don't "fix" a flake there by inflating the timeout. Each one also calls `wait_for_watcher_ready` first, which writes a probe file and waits for the watcher to report it: `notify` arms its stream on a background thread, so a write issued straight after `open_virtual` can land before anything is listening. The probe file is deliberately left in place, since deleting it queues a late `ObjectRemoved` that lands after the drain and steals the next test's first event.
- **Virtual device**: `cargo test -p mtp-rs --features virtual-device` (full protocol tests against local filesystem). `VirtualDeviceConfig` implements `Default`, so build it as `VirtualDeviceConfig { storages: vec![...], ..Default::default() }` and state only the fields a test actually exercises; new fields must land with a default so consumers don't break (see CONTRIBUTING.md). `VirtualStorageConfig` has no `Default` (an unset `backing_dir` fails silently). Fault injection: `force_partial_read_caps` (short/stall reads), `force_cancel_wedge` / `force_operation_wedge` (#18), `force_object_info_error` and `VirtualDeviceConfig::undescribable_objects` (partially-readable folders, #22). Capability shaping: `supports_partial_object` / `supports_partial_object_64` pick which partial-read ops the device advertises AND serves (both `false` models libhaze/Sphaira, where `download(ByteRange::Full)` is the only read).
- **Integration**: `cargo test -p mtp-rs --test integration -- --ignored --nocapture` (needs device). Destructive tests pick a writable root folder from a priority list (Android `Download`, Garmin `Music`, Kindle `documents`, etc.); set `MTP_TEST_FOLDER=Name` to override. See `crates/mtp-rs/tests/integration.rs` header for full details.
- **CLI**: `cargo test -p mtp-rs-cli --features virtual-device` (runs the built binary against a virtual device)
- **Property**: `cargo test --workspace --all-features` (proptest fuzzing)

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
`doctor` reports `unreadable_root_objects`; `collect_objects_recursive` aggregates skips across a
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
listings actually spend their 15+ seconds.

Streaming downloads keep using the SIC class-cancel path (see below); that's a
different mechanism for a different problem (one big bulk-IN to drain).

## Transfer cancellation

Mid-stream download cancellation uses the USB Still Image Class (SIC) cancel
mechanism: a CLASS_CANCEL control request (bRequest=0x64) followed by draining
the bulk IN and interrupt pipes. This approach was validated against libmtp's
`ptp_read_cancel_func` (Florent Viard, 2017). Key implementation notes:

- The drain must start **immediately** after CLASS_CANCEL: any delay (like
  polling GET_DEVICE_STATUS, which Android doesn't support) allows the device
  to enter an unrecoverable state.
- The drain uses maxpacket-sized reads with a 300ms idle timeout (matching
  libmtp and Windows behavior).
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
  backoff, not a tight loop). `download_windowed` removes the *need* to cancel
  (no held-open transfer to abort), but it does **not** remove the wedge: a
  window future dropped mid-flight still wedges the device through the recovery
  drain. It's the recoverable flavor, though, so prefer it on Android anyway.
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

## Reading large files without monopolizing the session

`Storage::download_windowed(handle, window_size)` reads a file as a SEQUENCE of
bounded `GetPartialObject64` transactions instead of one held-open stream. It
returns a `WindowedDownload` whose `next_window()` reads the next window and
RELEASES the one-per-device PTP session on return. Companions:
`download_windowed_from_offset` (resumable), `download_windowed_default`, the
`DEFAULT_DOWNLOAD_WINDOW` const (8 MiB).

The motivation is the session monopoly: `download` owns the single PTP session
for the WHOLE file, whatever the range, so
no other op (a folder listing, navigation) can touch the device until the read
finishes or is aborted. The spike numbers that drove this (validated on a Pixel
9 Pro XL): an 8 MiB window is ≈80ms and frees the session between windows, so a
concurrent listing slips in at its natural cost, versus ~35s to abort a
held-open multi-GB read (the USB cancel must drain the whole backlog), which
times out a concurrent listing. A downstream consumer (Cmdr) hit exactly this
and hand-rolled the window loop before it moved here.

Design boundary (important): `WindowedDownload` owns the BOOKKEEPING (cached
total size, current offset, window sizing, EOF detection) but NO policy: no
pause, debounce, or gate. The consumer interposes its own logic BETWEEN
`next_window()` calls. `window_size` is a real, open parameter; the 8 MiB default
is a documented suggestion, not baked in.

Edge-case contract (mirrors `download` with a `From` range): empty file /
`offset == size` ⇒ first `next_window()` returns `None` and issues no read;
`offset > size` ⇒ `Error::InvalidData` before any USB I/O; a 0-byte read while
`offset < size` ⇒ `Error::InvalidData` (a device STALL, not a silent EOF and not
a spin); a short non-zero mid-file read advances by the bytes ACTUALLY returned;
`window_size` is clamped to u32 (the `GetPartialObject64` `max_bytes`) and to ≥1.
`size()` reports the FULL object size so progress/ETA stays anchored.

Drop safety: unlike `FileDownload` (holds the session open, MUST be consumed or
`cancel()`led before drop), `WindowedDownload` holds NOTHING between windows, so
stopping early is just dropping it: no `cancel()`, `Drop` is a no-op. A
`next_window()` future dropped mid-call self-heals via `TransactionScope` (the
next op drains).

Tested against the virtual device in `transport/virtual_device/mod.rs`
(`windowed_download_*`), including the headline `..._session_free_between_windows`
(a `list_objects` succeeds between two `next_window()` calls) and the
`..._zero_bytes_before_eof_errors` stall path. The stall and short-read paths use
the `force_partial_read_caps(serial, caps)` virtual-device hook (caps the next
reads' returned length; 0 = empty/stall, n = short read). `examples/windowed_download.rs`
demonstrates it with no hardware. Real-device coverage:
`test_windowed_download_matches_stream_and_frees_session` in
`tests/integration.rs` (`#[ignore]`).

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
  reopens come first, and they're enough on a Pixel. Don't reword the reset docs
  back into "the recovery recipe". Evidence:
  [docs/notes/android-wedges-and-the-reset-kill-switch.md](docs/notes/android-wedges-and-the-reset-kill-switch.md).
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
- Tested against the mock with a controllable mid-receive suspension
  (`MockTransport::block_receive`) in `ptp/session/mod.rs`
  (`abandoned_receive_*`). The mock's `cancel_transfer` drains its queued
  responses to mirror a real pipe drain.

## Streaming uploads (USB bulk transfer details)

Uploads use `Transport::send_bulk_streaming()` to avoid buffering the entire
file in RAM. Key implementation notes:

- PTP data containers can span multiple USB bulk transfers. The device
  detects end-of-data via a short packet (< max packet size) or a
  zero-length packet (ZLP) when the total is a multiple of max packet size.
- Each `Endpoint::submit()` call is a separate USB transfer. The header
  (12 bytes) is prepended to the first chunk so the device sees the PTP
  container header in the first transfer (matching libmtp behavior).
- Data is batched into 256KB USB transfers using nusb's low-level
  `allocate/submit/wait_next_complete` API. `EndpointWrite` would be
  cleaner but requires ownership of the `Endpoint`, which lives behind
  a `Mutex` in `NusbTransport`.
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
virtual device mirrors real devices here: it creates the object (empty
placeholder) at `SendObjectInfo` time, so a partial upload leaves a real,
queryable, deletable handle.

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
speaks MTP itself and blocks the raw opcodes. See `docs/windows-wpd-backend-plan.md`. Quirks and
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
  `GetStream` → `Unsupported`/`NotFound`). Events (`next_event`) still return `Unsupported` (Phase 4).
- **Selection**: on Windows, `open_first`/`open_by_serial` default to WPD (`Backend::Auto`), falling
  back to USB when no WPD device is present; `Backend::Usb` forces PTP-over-USB (e.g. a Zadig-bound
  camera), `Backend::Wpd` forces WPD.

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
- [docs/releasing.md](docs/releasing.md): how to publish a new version to crates.io
- [docs/notes/android-wedges-and-the-reset-kill-switch.md](docs/notes/android-wedges-and-the-reset-kill-switch.md): what a day of hardware work established about wedged Android MTP sessions. The two wedge signatures (Samsung errors, Pixel hangs), the dropped-future trigger, why the transport reset kills a Pixel's MTP function, what recovers what, and which claims rest on a single observation. Read before changing reset or recovery guidance.
- [docs/notes/community-threads.md](docs/notes/community-threads.md): required reading before working on issues or PRs. Recap of every GitHub thread so far, known device quirks, and recurring contributors. Update after work that affects community-facing context.
- [MTP v1.1 Spec](https://github.com/vdavid/mtp-v1_1-spec-md)
