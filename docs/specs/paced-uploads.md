# Paced uploads (#62): research spike

Goal: find out whether an upload to an Android phone can go out in windows through AOSP's edit operations, so the
session is free between windows the way paced downloads free it (AGENTS.md § Paced downloads). This doc holds the
evidence; the design decision is open.

**Status: interim.** The hardware run stopped early on 2026-11-11: the probe wedged the Pixel's `MtpServer` in the
`combos` step (see § Hardware findings), quiet spaced reopens didn't bring it back, and it needs a replug. Window
cost, interleaving, `EndEditObject` read-back, re-keying, and the stop modes are still unmeasured. The probe for all of
them is ready: `crates/mtp-rs/examples/android_edit_probe.rs`.

## What we added

- `ptp::OperationCode::{SendPartialObject (0x95C2), TruncateObject (0x95C3), BeginEditObject (0x95C4),
  EndEditObject (0x95C5)}`.
- `PtpSession::{begin_edit_object, end_edit_object, truncate_object, send_partial_object}`. `send_partial_object`
  streams its data through `execute_with_send_stream`, so a stopped window takes the same ZLP path as a stopped
  `SendObject` (#62's comment), and returns the response's first parameter (AOSP: the byte count written).
- The probe, with `info`, `combos`, `bench`, `interleave`, `stop <mode>`, `check`, and `cleanup` commands. Its header
  says what each one does.

## Wire shapes (read from source)

From AOSP `frameworks/av` `media/mtp/MtpServer.cpp` on `main` (2026-11-11) and libmtp `src/ptp.c`:

- `SendPartialObject` 0x95C2: params `(handle, offset_lo, offset_hi, length)`, then a host-to-device data phase of
  `length` bytes. `GeneralError` if the object isn't open for editing or `offset > size` (append is fine, no holes).
  Writes with `pwrite`, then `receiveFile`. Response param 1 is the length written. A cancel answers
  `TransactionCancelled`. A failed window does not unlink the file.
- `TruncateObject` 0x95C3: `(handle, offset_lo, offset_hi)`, no data phase, `ftruncate`.
- `BeginEditObject` 0x95C4 / `EndEditObject` 0x95C5: `(handle)`, no data phase. Begin opens the file
  `O_RDWR | O_EXCL`, **no `O_CREAT`**, and answers `GeneralError` if the open fails or the object is already being
  edited. End runs `commitEdit`, which is `MtpDatabase.rescanFile` → `MediaStore.scanFile`: one MediaProvider scan
  at the end, none per window.
- When the server's run loop ends (session over, host gone), it commits every open edit.
- `GetObjectInfo` on an object under edit reports the edit's current size.
- libmtp sends `SendPartialObject` with `split_header_data`, with the comment that `MtpServer.cpp` used `write()`
  without an offset for the first chunk. On `main` the first chunk goes through `pwrite(..., offset)`, so the
  workaround looks unnecessary there (inferred; the probe uses combined mode and compares the bytes, which would show
  it).

How `SendObjectInfo` and `SendObject` set the object up (read from source):

- `doSendObjectInfo` creates **no file** for a regular object: it adds a cache entry (`beginSendObject`) and remembers
  `mSendObjectHandle`.
- `handleRequest` forgets `mSendObjectHandle` on **any** command other than `SendObject`.
- `doSendObject` creates the file (`O_CREAT | O_TRUNC`), so it's the only way to get a file the edit ops can open. With
  no pending `SendObjectInfo` it answers `NoValidObjectInfo` (0x2015) **without reading the data phase**.
- `receiveFile` with a declared size of 0 returns at once (no ZLP expected), so a 12-byte, header-only `SendObject`
  should be a valid way to create an empty file. Not confirmed on hardware yet.

So the inferred shape is `SendObjectInfo(size = 0)` → `SendObject` with no payload → `BeginEditObject` →
`SendPartialObject` × N → `EndEditObject`. The first two must be back to back with nothing in between.

## Hardware findings

Pixel 9 Pro XL, Android 17 (`CP3A.261005.005`), macOS/nusb, USB 3, 2026-11-11.

- **The phone lists all five ops**: `DeviceInfo` has 0x95C1–0x95C5, vendor extension
  `microsoft.com: 1.0; android.com: 1.0;` (1 run). `DeviceInfo::runs_android_mtp_server` already accepts either
  `android.com` or all of 0x95C3–0x95C5, so a device that passes it can be expected to have the edit ops, but a
  paced upload should check 0x95C2–0x95C5 itself rather than reuse that predicate.
- **`BeginEditObject` right after `SendObjectInfo` fails** (case A, 1 run, matches the source): `GeneralError`,
  logcat `open failed for …/mtprs-probe-combo-a.bin in doBeginEditObject (2)` (ENOENT). The object stays in the
  listing with size 0 (a cache entry with no file), `GetObject` answers `GeneralError`, and `DeleteObject` answers
  0x2012 `PartialDeletion` (`deletePath stat failed`). So skipping `SendObject` leaves a ghost entry for the rest of
  the session. Not checked yet whether it survives a new session.
- **A `SendObject` after the pending info was forgotten wedged the phone** (case B, 1 run). The probe had sent one
  `GetObjectInfo` between `SendObjectInfo` and `SendObject` (a probe bug, fixed since). The phone logged
  `Expected SendObjectInfo before SendObject` and answered 0x2015 without reading the data phase. Our host then sat
  in the bulk OUT send of the 12-byte data container until the 30 s timeout (inferred: nothing on the phone reads
  bulk OUT while it waits to send its response), and every later operation timed out too. The phone logged nothing
  more. Five spaced reopens over ~2.5 min, then one more after a minute of quiet: all `Timeout`. `android.process.media`
  stayed alive; no `MtpServer` exit in logcat. Needs a replug. The SIC reset wasn't tried (it kills a Pixel's MTP).

### Possible existing bug this points at (not verified)

`UsbBackend::upload` sends `SendObjectInfo` and `SendObject` as two separate session-lock acquisitions. A consumer
that lists a folder from another task while an upload starts (Cmdr does) can get a command in between, which makes
AOSP forget the info, and the `SendObject` then meets the same refusal-before-data as case B. If that reproduces, it's
a wedge in today's uploads, independent of paced uploads. Fix candidates: hold the session lock across both phases
(a `PtpSession` call that does both), and teach `execute_with_send*` to look for an early response when the data
phase stalls. Worth a test with a concurrent listing before anything else.

## Desk research

- **No other partial write op** (read from source): MTP 1.1 has `GetPartialObject` only. Microsoft's extensions
  (WMDRMPD, WMDRMND, WMPPD, `SendObjectPropList` 0x9808) have no offset write; MTPZ isn't in libgphoto2's tables.
  Camera vendors: libgphoto2 `camlibs/ptp2/ptp.h` names Canon `SendPartialObject` 0x9019, `EOS_SendPartialObject`
  0x910B, `EOS_SendPartialObjectEx` 0x913A / `EOS_EndSendPartialObjectEx` 0x913B, and Fuji `FmSendPartialObject`
  0x9042, but `ptp.c` has no wrapper for any of them, so their shapes are undocumented. Nothing comparable for Nikon,
  Sony, Olympus, or Panasonic. Cameras rarely take uploads, so these aren't worth chasing.
- **Samsung** (read from a public `mtp-detect` dump, Galaxy S23 Ultra SM-S918B, firmware S918BXXU8CYB4, 2025-03,
  https://forums.opensuse.org/t/no-mtp-compatible-devices-found/183573): vendor extension without `android.com`,
  operations include 0x95C1–0x95C5. Samsung's server is closed, so whether they behave like AOSP is unknown.
- **Xiaomi** (read from a public dump, Redmi Note 13 Pro+ 5G, 2026-04, https://discuss.cachyos.org/t/…xiaomi-13pro/27433):
  `microsoft.com: 1.0; android.com: 1.0;`, 0x95C1–0x95C5 and 0x9805. HyperOS source isn't public.
- **libmtp** decides per operation with `ptp_operation_issupported`; `LIBMTP_DEVICECAP_EditObjects` needs 0x95C3,
  0x95C4, and 0x95C5. No device flags gate it.
- **WPD**: the API has write streams and a resource seek command, but WpdMtpDr can only map them onto MTP ops, so an
  offset write into an existing object is unlikely except through Android's ops (inferred). The `MTP_EXT` passthrough
  could carry 0x95C2–0x95C5, and the #61 probe saw the driver list them on a Pixel, but `docs/specs/wpd-partial-reads.md`
  forbids writes through the passthrough: the driver caches object properties, and a write behind its back would make
  that cache wrong (size 0 until something refreshes it).

## Options (provisional, pending the measurements)

1. **USB, devices listing 0x95C2–0x95C5**: `SendObjectInfo(0)` + empty `SendObject` (one locked call) →
   `BeginEditObject` → windows sized by the download `Pacer` → `EndEditObject`. Cancel between windows sends nothing;
   mid-window it takes the existing ZLP path. A failure leaves a file holding a prefix (AOSP doesn't unlink on a
   failed window, and the run loop commits open edits when the session ends), so `UploadError::partial` would be
   `Some` again on Android, the opposite of today's `SendObject` behavior.
2. **WPD**: keep `Commit` for now. Passthrough writes need a way to refresh the driver's cache first; out of scope
   until option 1 has numbers.
3. **Devices without the edit ops** (cameras, Garmin, Kindle, Switch homebrew): no protocol way to interleave inside
   one `SendObject`. Host-side scheduling between files in a multi-file copy (a consumer concern, maybe a hint like
   `holds_session()` from us), and document the rest.

Recommendation so far: option 1 looks viable from the source, but don't commit to it before `bench` and `stop` run;
first, check the possible existing bug above, since it affects every Android upload today.

## Still to run (after the replug)

From `crates/mtp-rs`, with the ptpcamerad blocker running and adb stopped:

1. `combos`: cases B–D and the edge cases (second begin, offset past the end, rewrite, truncate).
2. `bench <1 GiB file> 1M 8M 32M 128M`: per-window cost against held-open `SendObject`; compare with #60's
   ~15–20 ms per download window.
3. `interleave <file> 32M`, `interleave <file> 128M`, `interleave <file> held`: caller latency during each.
4. `bench … --check-every 1 --ext mp4`: does the handle survive a long upload with a media extension.
5. `stop cancel <256 MiB file> 32M 10`, `stop drop … 10`, `stop noend-close … 3`, `stop noend-drop … 3`,
   `stop exit …` then `check` (3 times).
6. A concurrent listing between `SendObjectInfo` and `SendObject` through `MtpDevice::upload`, to confirm or rule out
   the existing bug.
