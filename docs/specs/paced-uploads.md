# Paced uploads (#62): research spike

Goal: find out whether an upload to an Android phone can go out in windows through AOSP's edit operations, so the
session is free between windows the way paced downloads free it (AGENTS.md § Paced downloads). This doc holds the
evidence and the options.

## Status

**Built (option 1), on by default where it keeps dates.** The design and its rules are in AGENTS.md § Paced
uploads; this doc keeps the evidence. Decisions (David, 2026-11-11):

- Windowed uploads on devices listing 0x95C2–0x95C5, everything else one `SendObject`, WPD keeps `Commit`.
- **The default is #68's option (a), "dates win"** ([#68](https://github.com/vdavid/mtp-rs/issues/68)):
  `UploadPacing::KeepDates` holds an upload open when it carries a date and the device keeps dates, and uses
  windows otherwise. Whether the device keeps dates is learned per session from one `GetObjectInfo` after the
  first held-open upload dated more than three days from now (no device list); a Pixel keeps them, a Galaxy S23
  Ultra drops them, so Samsung gets windows from its second dated upload on. An explicit pacing always wins.
  Windows lose the file's date on Android, and nothing can set it afterwards (§ Dates (#68)). The matching
  rule and its tolerances are in AGENTS.md § Paced uploads. Hardware-verified on the Pixel
  (`test_default_upload_pacing_keeps_dates`, § The built API on hardware); the Galaxy S23 Ultra run is still owed.
- **Upstream**: an AOSP patch lets `SetObjectPropValue(DateModified)` set the date, so a windowed upload could keep
  it ([bug 584828271](https://issuetracker.google.com/issues/584828271),
  [change 4435411](https://android-review.googlesource.com/c/platform/frameworks/base/+/4435411)). Host order
  with it: `EndEditObject`, then `SetObjectPropValue(DateModified)`. Not built here: no released phone has it.
- A stopped windowed upload returns `UploadError::partial: Some(handle)` with a clean prefix; the library doesn't
  delete it. On a stop: truncate to what was acknowledged, then `EndEditObject`.
- **A dropped windowed upload is deleted** by the next operation (truncate, end, delete): nobody holds its handle,
  and a truncated file under the real name looks complete. A deliberate exception to "never auto-delete".
- A step-by-step `FileUpload` (`Storage::start_upload`) mirroring `FileDownload`, for consumers with their own
  device lock; the stream-based uploads sit on top of it.
- The virtual device models the edit ops, refusals and ghost included.
- `PtpSession::send_object_with_info` (and the new `begin_edit_upload`) stay crate-private.
- The `cargo-semver-checks` lint allow for the new `OperationCode`s.
- Resumable uploads across sessions (option 2): not built.

Decided while building, open to change:

- **Dates can't be set afterwards, and a windowed upload can't keep its date on AOSP at all**: § Dates (#68) has the
  source and the Pixel runs. Samsung's answer to `SetObjectPropValue(DateModified)` is still untried (the S23 went
  back first).
- A free-space check (`GetStorageInfo`) before a windowed upload, since size 0 keeps the phone from refusing a
  too-big file up front.
- A held-open upload starts with its first chunk, so a `Break` before any byte went out creates nothing.

Hardware for every claim below unless it says otherwise: Pixel 9 Pro XL, Android 17 (`CP3A.261005.005`),
macOS/nusb, USB 3 (5 Gb/s), 2026-11-11. § Galaxy S23 Ultra repeats the probe on Samsung's server. The probe is `crates/mtp-rs/examples/android_edit_probe.rs`; its header lists
the commands. Each claim says how many runs it rests on.

## What we added

- `ptp::OperationCode::{SendPartialObject (0x95C2), TruncateObject (0x95C3), BeginEditObject (0x95C4),
  EndEditObject (0x95C5)}`.
- `PtpSession::{begin_edit_object, end_edit_object, truncate_object, send_partial_object}`. `send_partial_object`
  streams through `execute_with_send_stream`, so a stopped window takes the same ZLP path as a stopped `SendObject`.
- The probe, and the hardware test `test_upload_with_concurrent_listings` (`tests/integration.rs`).

## Wire shapes and server behavior (read from source)

AOSP `frameworks/av` `media/mtp/MtpServer.cpp` and `MtpFfsHandle.cpp` on `main` (2026-11-11), libmtp `src/ptp.c`:

- `SendPartialObject` 0x95C2: params `(handle, offset_lo, offset_hi, length)`, then `length` bytes of data. Writes with
  `pwrite`, then `receiveFile`. `edit->mSize` grows only when the whole window arrived.
- `TruncateObject` 0x95C3: `(handle, offset_lo, offset_hi)`, no data, `ftruncate`.
- `BeginEditObject` / `EndEditObject`: `(handle)`, no data. Begin opens the file `O_RDWR | O_EXCL` with **no `O_CREAT`**
  and refuses (`GeneralError`) a missing file or an object already being edited. End runs `commitEdit` =
  `MtpDatabase.rescanFile` → `MediaStore.scanFile`, one scan per upload.
- **Refused before the data phase is read** (the wedge below): `SendPartialObject` on an object not open for editing, or
  at an offset past the edit's size; `SendObject` with no pending `SendObjectInfo`.
- `handleRequest` forgets a pending `SendObjectInfo` on any other command. `doSendObjectInfo` creates no file;
  `doSendObject` does (`O_CREAT | O_TRUNC`), and with a declared size of 0 it reads only the 12-byte header.
- The edit list lives in the server, not the session: only the end of `MtpServer::run` (USB function gone) commits open
  edits. `OpenSession`/`CloseSession` don't touch them.
- Response parameter 1 of `SendPartialObject` is `length` minus the bytes that came with the first packet (a server
  bug), so it can't be used as a byte count: 1 MiB windows answered 1,032,204, a 1-byte write 0.
- libmtp sends `SendPartialObject` with split header and data, for an old `write()`-without-offset bug that `main`
  fixed with `pwrite`. Combined mode wrote correct bytes in every run below.

## Hardware findings

### The phone and the shape

- **`DeviceInfo` lists 0x95C1–0x95C5** with `microsoft.com: 1.0; android.com: 1.0;` (1 run).
  `DeviceInfo::runs_android_mtp_server` accepts either `android.com` or all of 0x95C3–0x95C5; a paced upload should
  check 0x95C2–0x95C5 itself.
- **Working shape** (combos case B, 2 runs, plus every windowed upload below, 30+): `SendObjectInfo(size = 0)` → a
  header-only `SendObject` (80 ms) → `BeginEditObject` (5 ms) → `SendPartialObject` × N → `EndEditObject`
  (16–400 ms, usually ~30). A second begin answers `GeneralError`, a rewrite inside the file and `TruncateObject`
  work, and a truncate is visible in `GetObjectInfo` at once.
- **`SendObject` can't be skipped** (cases A and D, 2 runs each, 1 more of A on the first day): `BeginEditObject`
  answers `GeneralError` (ENOENT). The object stays listed as a ghost: size 0, `GetObject` `GeneralError`,
  `DeleteObject` 0x2012. **The ghost survives new sessions and host processes** (seen across ~20 later processes); only
  an MTP restart clears it. The two in this run (`mtprs-probe-combo-a.bin`, `-d.bin`) have no file on disk and go
  away at the replug.

### Window cost

1 GiB of random data, three bench runs with the window sizes in different orders, each upload read back in full and
compared byte for byte:

- Held-open `SendObject`: 210, 231, 263 MB/s (4.1–5.1 s).
- 1 MiB windows: 121, 121, 125 MB/s; median window 7.5 ms against ~4.5 ms of data, so **~3 ms fixed cost per window**.
- 8 MiB: 121, 100, 189 MB/s; median 42–46 ms against 32–40 ms.
- 32 MiB: 191, 180, 252 MB/s; median 121–149 ms, about the data time.
- 128 MiB: 180, 191, 175 MB/s; median 519–567 ms, with one 1.3–1.4 s window per upload.

The spread comes from stalls (windows several times the median), not a fixed cost: the median cost of a window is a
few ms, below #60's 15–20 ms per download window. **A window sized to ~0.5–2 s (100–400 MiB at this speed) costs
about nothing**, and 32 MiB already ran at 72–96% of held open.

### Calls between windows

`interleave`: a second task lists `Download` (219 entries), reads one `ObjectInfo`, and downloads a 16 KB file every
250 ms during a 1 GiB upload (1 run each). Idle, those take 1 ms, 1 ms, and 6 ms.

- Held open: one round in 4.1 s, and its listing waited **2.9 s** (the rest of the upload).
- 32 MiB windows: 12 rounds; listing median 59 ms, max 377 ms; download median 143 ms, max 337 ms. Upload 181 MB/s.
- 128 MiB windows: 5 rounds; listing median 208 ms, max 961 ms; download max 1.36 s. Upload 190 MB/s.

A call waits for the window in flight, never longer. The uploader yields between windows; without that, the unfair
session lock lets the uploader take it straight back (seen in the race check below: 8 listings during 1,000 uploads).
These probe runs drove `PtpSession` directly; the built API needed more than a yield on a multi-thread runtime
(§ The built API on hardware).

### Read-back after `EndEditObject`

- Same handle before, during, and after (every windowed upload: 30+ runs, including `.mp4` and `.jpg` names). With
  `--check-every 1`, `GetObjectInfo` answered with the written size after each of 160 windows (two 1 GiB `.mp4`
  uploads): **no re-key mid-upload**.
- Size right in every run. Content equal byte for byte (30+), and SHA-256 on the phone equal to the source (one
  256 MiB upload, `sha256sum` over adb). MediaStore had the file at its full size afterwards (1 run).
- **The modification time is the time of the last write**, not the `DateModified` sent in `SendObjectInfo`. The header-only
  `SendObject` stamps the sent date (`futimens`), and the writes after it move it to now. A held-open upload keeps the
  sent date. Setting it afterwards is refused (§ Dates (#68)).

### Stopping partway

256 MiB file, 32 MiB windows, two whole windows (64 MiB) then a stop inside the third:

- **No `EndEditObject`, then `CloseSession` and a new session** (3 runs), or the session dropped with no close (3 runs),
  or the host process exiting at a window boundary (1 run): the object keeps its handle and exactly the 64 MiB
  written. **The edit stays open in the server**: `BeginEditObject` from the new session answers `GeneralError`, and
  `EndEditObject` from it works. So a new session can finish a stranger's edit, and the open file descriptor leaks
  until something ends it or MTP restarts. Deleting an object under edit works and leaves that edit behind.
- **Cancel mid-window** (a source error, the `Break` path; 10 runs): `send_partial_object` returned 54–87 ms after the
  stop, the next operation took 0.3–1.6 ms, the edit stayed open, and resuming at the edit's size worked. The edit's
  size stays at the last whole window, but **the bytes of the stopped window that reached the disk stay there**: after
  resuming 4 MiB and ending, the file was 81,805,300 bytes, not 71,303,168 (10 of 10, identical). A resume that writes
  to the end overwrites them; an abandoned upload has to truncate to the edit's size first.
- **Dropped future mid-window** (10 runs, 8 really mid-window): the next operation took 5–15 ms (lazy recovery), the
  edit stayed open, the resume worked, and the same leftover tail showed (94–98 MB files).
- **Process exit mid-window** (3 runs, then 1 through the CLI): the phone answered the next process's first command
  as data, ended the window short, and replied `GeneralError` to it. Through the CLI (`MtpDevice` opens with
  `OpenSession`), the open recovered at once (1 run). Through a raw `PtpDevice::get_device_info` (session-less) the first
  call failed with `GeneralError`, and an open 5 s later worked (3 of 3). Edit open afterwards, ended from the new
  session, tail bytes on disk as above.
- No stop mode wedged the phone (27 runs).

### The `SendObjectInfo`/`SendObject` race, on hardware

- **With the fix** (`MtpDevice::upload`, `test_upload_with_concurrent_listings`): 1,000 of 1,000 uploads, but only 8
  listings got in. With a 20 ms pause after each upload, 400 of 400 uploads and 146 listings.
- **The old two-call shape** (`MTP_TEST_INTERLEAVE_SPLIT=1`, raw `ptp::` session, 1 run): upload 42 hit the gap.
  Logcat: `Expected SendObjectInfo before SendObject`, `0x2015 in command MTP_OPERATION_SEND_OBJECT`; the host timed
  out after 30 s and the phone stopped answering: spaced reopens over 2 min all timed out. Same wedge as the first
  day's case B. Needs a replug.

## Galaxy S23 Ultra (Samsung's server)

Hardware: Galaxy S23 Ultra SM-S918B, firmware `S918BXXUAGZIF`, macOS/nusb, USB 3, 2026-11-11, the same probe
(`--serial R5CWA2C1VKB`, with the Pixel on the same host left untouched). Samsung's MTP server is closed source, so
this is the only evidence for it. **It behaved like AOSP in every case tried**; the differences are a faster link and
the dates.

- **`DeviceInfo`** (1 run): `microsoft.com: 1.0; samsung.com/kies: 6.0; samsung.com/devicestatus: 0;`, no
  `android.com`, lists 0x95C1–0x95C5 (plus 0x9501–0x9503, 0x9801–0x9805, 0x9811). Its `DeviceInfo` serial is a
  32-digit hex string, not the USB serial, so a host matching devices by serial must use the USB one. The first open
  after a quiet spell saw zero storages once and worked 5 s later (§ Android gotchas in `docs/debugging.md`).
- **Working shape** (combos case B, 4 runs, `.bin`, `.mp4`, `.jpg`, plus every windowed upload below, 40+): same
  answers as the Pixel. Header-only `SendObject` 3–49 ms, `BeginEditObject` 2–4 ms, `EndEditObject` 9–49 ms
  (usually ~12). Second begin `GeneralError`, rewrite inside the file and `TruncateObject` work, truncate visible at
  once, and response parameter 1 of `SendPartialObject` has the same bug (1,032,204 for 1 MiB, 0 for a 1-byte write),
  so the server is AOSP-derived here.
- **`SendObject` can't be skipped** (cases A and D, 1 run each): `BeginEditObject` `GeneralError`, ghost object listed
  with size 0 and date 1970-01-01, `GetObject` `GeneralError`, `DeleteObject` 0x2012, and `EndEditObject` on it
  `GeneralError`. Same as the Pixel, so presumably gone at the next MTP restart. The two from this run
  (`mtprs-probe-combo-a.bin`, `-d.bin`) were still listed when the session ended; not yet checked after a replug.
- **Window cost** (1 GiB, three bench runs, window sizes in three orders, every upload read back and compared byte for
  byte): held open 341, 342, 341 MB/s (3.15 s). 1 MiB windows 236, 232, 232 MB/s, median 4.2 ms against 3.1 ms of
  data, so **~1.3 ms fixed cost per window**. 8 MiB 313, 311, 316 MB/s (~2 ms per window). 32 MiB 328, 331, 293.
  128 MiB 342, 336, 337 (no measurable cost). Far steadier than the Pixel: the slowest 8 MiB window was ~2.5× the
  median, and no window stalled for a second. At this speed a 2 s window is ~680 MiB.
- **Calls between windows** (`interleave`, 1 run each; idle: listing 612 entries 1.5 ms, info 1.2 ms, 10 KB download
  10–14 ms): held open, one round in 3.25 s and its listing waited **1.98 s**. 32 MiB windows: 8 rounds, listing
  median 47 ms (max 62), download median 106 ms (max 114), upload 325 MB/s. 128 MiB: 6 rounds, listing median 146 ms
  (max 153), download max 397 ms, upload 334 MB/s. A call waits for the window in flight, never longer.
- **Read-back**: same handle before, during, and after every windowed upload (40+), and in a fresh listing after
  `EndEditObject` (combos, 4 runs, including `.mp4` and `.jpg`). `--check-every 1` over two 1 GiB `.mp4` uploads in
  8 MiB windows: `GetObjectInfo` answered with the written size after each of 256 windows, **no re-key**. Size and
  content right in every run. No on-phone SHA-256 (no adb on this phone); the byte-for-byte read-back covers it.
- **Dates differ from the Pixel: Samsung ignores the `DateModified` sent in `SendObjectInfo` for every upload**, held
  open included (5 held-open uploads, all windowed ones): the object reads the time of the upload. On the Pixel a
  held-open upload keeps the sent date. So setting a date needs `SetObjectPropValue(DateModified)` on both phones
  (the Pixel refuses it, § Dates (#68); Samsung untried), and Samsung's held-open uploads already lose the date today.
- **No `EndEditObject`, then `CloseSession` and a new session** (3 runs) or the session dropped with no close (3 runs):
  same handle, exactly the 64 MiB of the whole windows, `BeginEditObject` from the new session `GeneralError` (the edit
  is still open), `EndEditObject` from it OK (3 of 3). The three `CloseSession` runs deleted their objects without
  ending the edit, so three edits stay open in the server until MTP restarts.
- **Cancel mid-window** (source error, 32 MiB windows, 10 runs): `send_partial_object` returned 48–56 ms after the
  stop, the next operation took 0.3–0.8 ms, the edit stayed open, a resume at the edit's size worked, and the file
  ended at 81,805,300 bytes (10 of 10, the Pixel's exact number): the stopped window's tail stays on disk. While the
  edit is open, `GetObjectInfo` reports the edit's size (64 MiB), not the bytes on disk.
- **Truncate, then end** (the abandoned upload, new `stop abandon`, 5 runs): stop mid-window, `TruncateObject` to the
  edit's size (3–8 ms), `EndEditObject` (5–12 ms). The object kept its handle and held exactly the 64 MiB of whole
  windows, equal to the file's first bytes, and a new `BeginEditObject` succeeded, so the edit was closed (5 of 5).
- **Dropped future mid-window** (64 MiB windows so the 100 ms drop lands inside one, 10 runs, all mid-window: 26–34 MB
  of the window on disk): next operation 2.7–12 ms, edit open, resume worked, same leftover tail.
- **Process exit mid-window** (64 MiB windows, 3 runs): the next process's `GetDeviceInfo` got `GeneralError` once and
  the open 5 s later worked (3 of 3, the Pixel's signature); the edit was open with the whole windows' size, and
  `EndEditObject` from the new process committed it with the tail included (163–168 MB). A `SendObjectInfo` for a name
  that already exists answers `GeneralError` (1 run, harmless).
- **No stop mode wedged the phone** (34 runs), and nothing needed the reset. The session answered after every
  experiment.

## Dates (#68)

**A windowed upload can't end with the source's date on AOSP.** Pixel 9 Pro XL, Android 17, macOS/nusb, 2026-11-11,
`android_edit_probe setdate` and `dates` (one run each, every object checked through MTP and with `stat` on
`/sdcard/Download/...` over SSH, which reads the file system's mtime).

From source (`frameworks/av` `MtpServer.cpp`, `MtpUtils.cpp`; `frameworks/base` `MtpDatabase.java`,
`MtpStorageManager.java`, `android_mtp_MtpDatabase.cpp`, all on `main`, 2026-11-11):

- The one `futimens` in the server is at the end of `doSendObject`, with the date from the pending `SendObjectInfo`.
  `doSendObject` opens the file `O_RDWR | O_CREAT | O_TRUNC`, so it can't run after the windows without wiping them.
- `setObjectProperty` handles `ObjectFileName` (a rename) and answers `ObjectPropNotSupported` for the rest, and
  `getObjectPropertyDesc` marks only `ObjectFileName` writable (`DateModified` is a read-only DateTime string).
- `copyFile` (behind `CopyObject`) writes a new file and copies no times, so a copy is dated now.
- The date MTP reports is `File.lastModified()` read at each request (`MtpObject.getModifiedTime`), so MTP and the
  file system can't disagree.

On the phone (MTP and `stat` agreed on every object):

- **Windowed, nothing after**: dated at the last write. `SetObjectPropValue(DateModified)` answers 0xA80A
  (`ObjectPropNotSupported`) for `20200102T030405`, `...Z`, and `...+0100`, on a windowed and on a held-open object, and
  changes nothing.
- **A second `SendObjectInfo` for the same name** (size 0, never followed by a `SendObject`): `GeneralError`.
- **Windowed into a temporary name, then renamed**: the rename works and keeps the upload's date.
- **Held open, then `BeginEditObject` + `EndEditObject` with no write**: keeps the sent date, so neither the edit
  nor its `MediaStore` scan touches the date. **Held open, then an edit that only truncates to the same size**:
  dated now (`ftruncate`).
- Rechecked with `stat` 5 s and 60 s later: no change, so the media scan doesn't restore or reset anything.

The options #68 weighed (David chose (a), built as `UploadPacing::KeepDates`):

- (a) Dates win by default: held open when the upload carries a date and the device keeps dates, windows
  otherwise (no date, or a device like Samsung that drops it anyway). The only option that loses no date a device
  would have kept.
- (b) Windows by default and dates lost.
- (c) A size threshold.
- (d) An AOSP patch: let `setObjectProperty` accept `DateModified` (`File.setLastModified`) and mark it writable in
  the prop desc, a few lines in `MtpDatabase.java`. It would also let a host fix Samsung's held-open uploads if
  Samsung picked it up. Years to reach phones, so it only comes with one of the others. Proposed upstream as
  [change 4435411](https://android-review.googlesource.com/c/platform/frameworks/base/+/4435411)
  ([bug 584828271](https://issuetracker.google.com/issues/584828271)); a host would send
  `SetObjectPropValue(DateModified)` after `EndEditObject`, since the edit's writes move the date.
- (e) The app decides per transfer.

## The built API on hardware

Pixel 9 Pro XL, Android 17, macOS/nusb, USB 3 (5 Gb/s), 2026-11-11 evening, the built `UploadPacing` /
`FileUpload` code in a test folder under `Download`.

- **`test_paced_upload_round_trip_with_listings`** (1 GiB, held open then `MaxWindowTime(2 s)`, a listing every
  250 ms from another task on tokio's multi-thread runtime): content right in every run (6 held open, 16
  windowed), held open kept the sent 2020 date, windowed was dated at the last write. Windows run first 8 MiB, then
  350–560 MiB each (1.3–1.9 s), 242–334 MB/s against 283–306 held open.
- **A listing waited two or three windows.** With a yield between windows (3 uploads), each listing that started
  mid-window waited 2.97–3.38 s; with a first fix that let one waiting request in (9 uploads), most still waited
  2.5–3.6 s. Two causes, both on the multi-thread runtime only: the woken caller sat behind the uploader in the
  worker's LIFO slot, and an Android listing is about six requests (one `GetObjectPropList` per property), and
  between them the uploader, woken on another worker, took the lock back. Instrumented: the listing's first request
  got in after the window, its second waited the next whole window. The virtual device showed the same (80–255
  windows of 64 KiB). Fixed with `OperationLock::let_waiting_calls_in` (AGENTS.md § Paced downloads), after which
  12 of 12 listings waited only for the window in flight (1.28–1.65 s, windows of 1.4–1.9 s), at the same
  throughput. Downloads share the fix; their virtual-device test failed the same way, but they weren't rerun
  under contention on hardware.
- **`test_upload_stopped_midway_then_session_works`** with `MTP_UPLOAD_PACING=time:2000`, 256 MiB, stops at 10%
  and 50% (inside the second window), 20 runs per mode, no wedge, no reopen, no reset in any of the 60:
  - `break`: returned 0.03–0.14 s after the break with `partial: Some`, holding exactly the first 8 MiB window as a
    clean prefix; next listing 0.01–0.03 s, delete 0.06–0.12 s (20 of 20).
  - `drop` (the future dropped while it waits on its source): the next listing, which also truncates, ends the edit,
    and deletes, took 0.10–0.27 s, and the object was gone through MTP and on the file system (20 of 20).
  - `timeout` (dropped 300 ms in, a USB transfer in flight): next listing 0.08–0.20 s, object gone (20 of 20).
- **The CLI keeps dates** (#69, #70, 1 run each): `put` of a file dated 2019-06-14 10:23 local gave the same epoch
  in `ls --json` (`2019-06-14T10:23:00`), `ls -l`, and `stat` on the phone; `get` of it and of four existing
  files (summer and winter dates) set the host mtime to the phone's to the second; a folder `get` dated the folder
  too.

2026-11-12, same phone, from an M1 Max host (macOS 27.0.1, nusb, 5 Gb/s), with `UploadPacing::KeepDates` the
default:

- **`test_default_upload_pacing_keeps_dates`** (16 MiB each, 2 runs): both dated uploads went held open and came
  back dated 2019-06-14 12:23 (the sent 10:23 UTC as phone-local CEST), the undated one went in windows, and the
  session's verdict was "keeps upload dates: true".
- **CLI `put`** with the new default (4 KiB and 50 MB files dated 2019-06-14 10:23 local, 1 run): both kept the date
  in `ls --long` and `--json`, and `get` brought it back to the host file, bytes identical.
- **`test_paced_download_with_listings_from_another_task`** (1 GiB, a listing every 250 ms from another task, 2
  runs): held open, the one listing waited 8.0–8.6 s; default pacing, 4 listings, median 2.0–2.3 s, max 2.3–2.5 s,
  so one window each. Throughput is the test's own (it checks every byte): 124–132 MB/s either way.
- **`test_paced_upload_round_trip_with_listings`** (1 run, after the call-guard fix): held open 204 MB/s, the
  listing waited 5.1 s; `MaxWindowTime(2 s)` 210 MB/s, 3 listings, median 1.76 s, max 1.79 s. Held open kept the
  2020 date, windowed was dated now.

## Desk research

- **No other partial write op** (read from source): MTP 1.1 has `GetPartialObject` only. Microsoft's extensions
  (WMDRMPD, WMDRMND, WMPPD, `SendObjectPropList` 0x9808) have no offset write; MTPZ isn't in libgphoto2's tables.
  libgphoto2 `camlibs/ptp2/ptp.h` names Canon `SendPartialObject` 0x9019, `EOS_SendPartialObject` 0x910B,
  `EOS_SendPartialObjectEx` 0x913A / `EOS_EndSendPartialObjectEx` 0x913B and Fuji `FmSendPartialObject` 0x9042 with no
  wrapper, so their shapes are undocumented. Nothing for Nikon, Sony, Olympus, or Panasonic.
- **Samsung** (read from a public `mtp-detect` dump, Galaxy S23 Ultra SM-S918B, firmware S918BXXU8CYB4, 2025-03,
  https://forums.opensuse.org/t/no-mtp-compatible-devices-found/183573): no `android.com`, lists 0x95C1–0x95C5. Its
  server is closed; § Galaxy S23 Ultra has the hardware results.
- **Xiaomi** (read from a public dump, Redmi Note 13 Pro+ 5G, 2026-04,
  https://discuss.cachyos.org/t/…xiaomi-13pro/27433): `android.com`, 0x95C1–0x95C5. HyperOS source isn't public.
- **libmtp** checks per operation (`ptp_operation_issupported`); `LIBMTP_DEVICECAP_EditObjects` needs 0x95C3–0x95C5.
- **WPD**: the driver lists 0x95C1–0x95C5 on a Pixel (#61's probe), so the passthrough could carry them, but
  `docs/specs/wpd-partial-reads.md` forbids writes through it: WpdMtpDr caches object properties, and an object
  written behind its back would read as size 0 until something refreshes it. Not tried.

## Options

1. **Windowed upload over USB on devices that list 0x95C2–0x95C5.** `SendObjectInfo(0)` + header-only `SendObject` (one
   hold of the lock), `BeginEditObject`, windows sized by the download `Pacer` (`MaxCancelDelay`, 2 s default), a yield
   between windows, `EndEditObject`. Rules the hardware set:
   - Never send a window the server would refuse before reading it: track the edit's size host-side and write only at
     offsets ≤ it, on an edit this host opened. A failed window leaves the size where it was, so the resume offset is
     known.
   - On any stop (cancel, error, drop), end the edit when the session still works: `TruncateObject` to the edit's size
     (drops the stopped window's tail), then `EndEditObject`. Without that, the edit leaks in the server until a
     replug.
   - `UploadError::partial` becomes `Some(handle)` on Android for a windowed upload: the file holds a clean prefix, so
     the caller can delete it or resume. The library doesn't delete it. That's the opposite of today's held-open
     Android upload (`None`), so it needs a changelog line and a consumer heads-up (Cmdr).
   - A windowed upload ends dated now: the phone refuses setting `DateModified` afterwards, and AOSP has no other way (§ Dates (#68)).
   - Cancel between windows sends nothing; mid-window it takes the existing ZLP path (10 of 10 clean).
2. **Resumable uploads across sessions** fall out of the same ops: an edit survives the session, and a new host can
   `EndEditObject` it, or `BeginEditObject` a committed prefix and append. Could be a later API
   (`upload_from(handle, offset)`), not part of the first cut.
3. **WPD**: keep `CreateObjectWithPropertiesAndData` + `Commit`. Passthrough edit ops would need the driver's cache
   refreshed after `EndEditObject`; a probe could tell, but it's a second project.
4. **Devices without the edit ops** (cameras, Garmin, Kindle, Switch homebrew): one
   `SendObject` per file, so the session stays held for each file. The consumer can free it between files of a
   multi-file copy; `holds_session()`-style hints on uploads would help Cmdr schedule. Document the rest.

**Recommendation**: build option 1, opt-out like `use_object_prop_list(false)`, and make it the default on devices
listing 0x95C2–0x95C5. The Galaxy S23 Ultra ran the probe clean (34 stop runs, no wedge, identical answers); the POCO
hasn't yet, and it's the one HyperOS data point left. The cost is small (a few ms per window on the Pixel, ~1–2 ms on
the S23; 2 s windows lose under 1%), the session frees within one window, the handle is stable, and every stop mode was
clean in 61 runs across both phones. The two risks are both host-side rules (never send a window the server refuses unread; always end the
edit), and both are testable on the virtual device if it models the edit ops.

## Open decisions

1. `UploadError::partial: Some` on Android for windowed uploads, where held-open uploads give `None` today.
2. Default on, or opt-in for one release, on edit-capable devices.
3. Whether to model the edit ops in the virtual device (needed to test the host rules without hardware).
4. Whether `PtpSession::send_object_with_info` becomes public for `ptp::` callers, who still have the race.
5. Resumable uploads across sessions (option 2): now, later, or never.
