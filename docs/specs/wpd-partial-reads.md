# WPD partial reads through the MTP passthrough (#61)

Status: implemented (`crates/mtp-rs/src/mtp/backend/wpd/passthrough.rs`; how it behaves now is in `AGENTS.md` § Windows WPD backend). This doc keeps the research and the probe (`crates/mtp-rs/examples/wpd_partial_probe.rs`, run on a Pixel 9 Pro XL on 2026-11-08, § Results). The shipped shape follows § If it works, with a cancel that reads the rest of the window, and the MTP handle checked once per object with a passthrough `GetObjectInfo`. End-to-end numbers through the public API: `crates/mtp-rs/examples/paced_with_calls.rs`, results in `AGENTS.md`.
Tags on claims: **[verified]** (we ran it), **[docs]** (Microsoft documentation or sample), **[code]** (someone
else's working code), **[guess]** (reasoning, untested).

## Problem

- On WPD the default-resource `IStream` of an Android phone is forward-only: `Seek` returns `E_NOTIMPL` (Pixel 9 Pro XL)
  **[verified]**. `stream_seek` falls back to read-and-discard, so reopening at offset N costs reading N bytes.
- Paced downloads (`AGENTS.md` § Paced downloads) read in windows and let other requests run between them. On WPD
  an open stream keeps the device busy, so the worker releases it before any other request (`AGENTS.md` § Windows
  WPD backend). Every window after another call then reopens the stream and re-reads the prefix: O(offset) per window,
  O(n²) per file. #61: 1 GB at 6.9 MB/s with 20 calls in between, 0.7 MB/s with ~200.
- Fix under evaluation: real partial reads, by sending MTP `GetPartialObject64` (0x95C1) or `GetPartialObject`
  (0x101B) through the Windows MTP driver's passthrough. Each window is then its own MTP transaction with no stream
  to keep, so other requests between windows cost nothing.

## The MTP passthrough commands

- All sent with `IPortableDevice::SendCommand(0, params)`. Only the WPD MTP class driver (WpdMtpDr) implements them
  **[docs]**: [Supporting MTP Extensions](https://learn.microsoft.com/en-us/windows/win32/wpd_sdk/supporting-mtp-extensions).
- Every parameter bag carries `WPD_PROPERTY_COMMON_COMMAND_CATEGORY` (the command's `fmtid`, a GUID) and
  `WPD_PROPERTY_COMMON_COMMAND_ID` (its `pid`, VT_UI4). Errors come back in the results as
  `WPD_PROPERTY_COMMON_HRESULT`, not as `SendCommand`'s return value **[docs]**:
  [IPortableDevice::SendCommand](https://learn.microsoft.com/en-us/windows/win32/api/portabledeviceapi/nf-portabledeviceapi-iportabledevice-sendcommand).
- Commands (category `WPD_CATEGORY_MTP_EXT_VENDOR_OPERATIONS`, GUID `4d545058-1a2e-4106-a357-771e0819fc56`):
  - `WPD_COMMAND_MTP_EXT_GET_SUPPORTED_VENDOR_OPCODES` (pid 11)
  - `WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITHOUT_DATA_PHASE` (pid 12)
  - `WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_READ` (pid 13)
  - `WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_WRITE` (pid 14)
  - `WPD_COMMAND_MTP_EXT_READ_DATA` (pid 15), `WPD_COMMAND_MTP_EXT_WRITE_DATA` (pid 16)
  - `WPD_COMMAND_MTP_EXT_END_DATA_TRANSFER` (pid 17)
  - `WPD_COMMAND_MTP_EXT_GET_VENDOR_EXTENSION_DESCRIPTION` (pid 18)
- Every one of these, and every property below, exists in `windows` `=0.62.2` (our pin) under
  `windows::Win32::Devices::PortableDevices`, behind the `Win32_Devices_PortableDevices` feature we already enable
  **[verified]** (grepped the crate source; the probe compiles against it). No new `windows` features, no new
  dependencies, no `[dev-dependencies]` change: an example sees the crate's `cfg(windows)` dependencies.

### Call sequence for one partial read

Per [Issuing the GetDevicePropValue Command](https://learn.microsoft.com/en-us/windows/win32/wpd_sdk/issuing-the-getdevicepropvalue-command),
the [EXECUTE_COMMAND_WITH_DATA_TO_READ reference](https://learn.microsoft.com/en-us/windows/win32/wpd_sdk/wpd-command-mtp-ext-execute-command-with-data-to-read),
and [dimeby8's part 3](https://learn.microsoft.com/en-us/archive/blogs/dimeby8/sending-mtp-commands-through-wpd-part-3-data-from-device) **[docs]**:

1. `EXECUTE_COMMAND_WITH_DATA_TO_READ`:
   - In: `WPD_PROPERTY_MTP_EXT_OPERATION_CODE` (VT_UI4, the opcode), `WPD_PROPERTY_MTP_EXT_OPERATION_PARAMS`
     (`IPortableDevicePropVariantCollection` of VT_UI4, set with `SetIPortableDevicePropVariantCollectionValue`).
     0x95C1: `[handle, offset_lo, offset_hi, max_bytes]`. 0x101B: `[handle, offset_u32, max_bytes]`.
   - Out: `WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT` (LPWSTR cookie), `WPD_PROPERTY_MTP_EXT_TRANSFER_TOTAL_DATA_SIZE`
     (reference page says VT_UI8, Microsoft's sample reads a ULONG; read UI8, fall back to UI4), and optionally
     `WPD_PROPERTY_MTP_EXT_OPTIMAL_TRANSFER_BUFFER_SIZE` (VT_UI4). A total of 0xFFFFFFFF means unknown: read until a
     short chunk.
2. `READ_DATA`, repeated until the total is in:
   - In: the context, `WPD_PROPERTY_MTP_EXT_TRANSFER_NUM_BYTES_TO_READ` (VT_UI4), and an input buffer of that size
     in `WPD_PROPERTY_MTP_EXT_TRANSFER_DATA` (`SetBufferValue`). The buffer is required **[docs]**.
   - Out: the bytes in `WPD_PROPERTY_MTP_EXT_TRANSFER_DATA` of the *results* (`GetBufferValue`, COM-allocated, free
     with `CoTaskMemFree`), plus `..._TRANSFER_NUM_BYTES_READ`. The input buffer isn't written to **[docs]**.
   - Microsoft says to chunk transfers over ~256 KiB by the optimal size so the app stays responsive **[docs]**.
     digiCamControl does exactly that for camera image downloads (`ExecuteReadBigData` in
     [`PortableDeviceLib/StillImageDevice.cs`](https://github.com/dukus/digiCamControl/blob/master/PortableDeviceLib/StillImageDevice.cs)) **[code]**.
3. `END_DATA_TRANSFER`:
   - In: the context. Out: `WPD_PROPERTY_MTP_EXT_RESPONSE_CODE` (VT_UI4) and `..._RESPONSE_PARAMS` (collection;
     param 1 of a partial read is the byte count actually sent).
   - A device that refuses the operation skips the data phase (total 0) and the refusal shows up here **[docs]**.
- If `SendCommand` answers `E_ACCESSDENIED`, set `WPD_API_OPTION_IOCTL_ACCESS` to `FILE_READ_ACCESS |
  FILE_WRITE_ACCESS` (3) in the parameters and resend **[docs]** (SendCommand remarks; whether these commands need it
  is unknown, the probe retries automatically and says so).

## Does WpdMtpDr let standard opcodes through?

Verdict: **probably yes** (my confidence ~75%), pending the probe.

- For: Microsoft's own example of this API sends a *standard* PTP opcode, `GetDevicePropValue` (0x1015), not a vendor
  one **[docs]** (linked above).
- For: kon0524/WpdMtp, a C# library built on this passthrough, runs `GetDeviceInfo`, `GetStorageIDs`,
  `GetObjectHandles`, `GetObjectInfo`, and **`GetObject` (0x1009, a file download)** through it on a RICOH THETA S
  ([`Test/Program.cs`](https://github.com/kon0524/WpdMtp/blob/master/Test/Program.cs)) **[code]**. Its README notes
  Windows does `OpenSession`/`CloseSession` itself (their note says unconfirmed).
- For: digiCamControl downloads camera images through it, chunked **[code]**.
- No source found that describes an opcode allowlist or a blocked opcode. The parameter docs call the opcode
  "vendor-extended", but the sample contradicts that. Absence of evidence, so not certain.
- Against / unknown: every working example is a camera, none is Android. 0x95C1 is in the vendor range and Android
  advertises it in `DeviceInfo`, so the driver has no reason to treat it specially **[guess]**.
  `GET_SUPPORTED_VENDOR_OPCODES` should list it if the driver parsed the device's operations **[guess]**; the probe
  prints that list.
- Unknown: throughput. Each `READ_DATA` crosses into the user-mode driver host (WUDFHost) and carries an input
  buffer the size of the read, so small chunks may be slow and big ones cost an extra copy **[guess]**. The probe
  measures 0x95C1 at several chunk sizes against one plain stream pass.

## Getting the MTP handle from a WPD object id

- WpdMtpDr names objects `o<hex handle>` and storages `s<hex storage id>`: we saw `s10001` and `o2C`-style ids on
  the Pixel (`ids.rs` fixtures) **[verified, Pixel only]**, and WpdMtpLib parses `ObjectAdded` event ids with
  `uint.Parse(id.Trim('o'), HexNumber)` (`WpdMtpLib/MtpCommand.cs`) **[code]**. It's undocumented, so it's a driver
  detail that could change.
- `WPD_OBJECT_PERSISTENT_UNIQUE_ID` is not the handle: on MTP it carries the object's persistent unique id (PUOID,
  property 0xDC41) **[guess, from memory of the WPD docs]**. The probe prints it next to the id.
- The probe checks the mapping: passthrough `GetObjectInfo` (0x1008) on the parsed handle must return the WPD
  filename and size. In production: parse `o<hex>`, verify once per object with 0x1008 (or trust it and let a wrong
  handle fail the first window, then fall back to the stream), and use the stream path when the id doesn't parse.

## Session and transaction concerns

- The passthrough runs inside the driver's own MTP session: no `OpenSession` from us, the driver numbers the
  transactions **[docs]** (the samples never open a session) **[code]** (WpdMtpLib's README). Never send
  `OpenSession`/`CloseSession` through it.
- A started data phase must be read to the end, "otherwise, we run the risk of the device going out of sync with
  the driver" **[docs]**. So a cancel mid-window drains the rest of the window (≤ the pacing target, about 2 s) and
  then sends `END_DATA_TRANSFER`. Same bound as USB on Android (§ Transfer cancellation). Whether an early
  `END_DATA_TRANSFER` cancels cleanly is unknown; `--early-end` tests it.
- `IPortableDevice::Cancel` from another thread can cancel a pending `SendCommand` **[docs]**; what that does to an
  MTP transaction is unknown. Not needed if windows are bounded.
- Whether other WPD calls (ours, Explorer's) wait or fail while a passthrough transfer is open is unknown; our worker
  is single-threaded, so it never interleaves its own calls anyway. `--interleave` tests both directions.
- WpdMtpDr caches object properties; read-only passthrough ops don't change anything, so no cache to invalidate
  **[guess]**. Writes through the passthrough would, so don't.

## Alternatives if the passthrough is blocked or slow

- **Raw resource commands.** `IStream` over WPD is a wrapper around `WPD_COMMAND_OBJECT_RESOURCES_OPEN/READ/SEEK/CLOSE`.
  The `E_NOTIMPL` might come from the API layer and not the driver **[guess]**. The probe sends `SEEK` directly through
  `SendCommand` and compares the bytes read after it. If that works, the fix is a small change to `stream_seek`.
- **No `CAN_SEEK` attribute exists.** `windows` 0.62.2 has `WPD_RESOURCE_ATTRIBUTE_{TOTAL_SIZE, CAN_READ, CAN_WRITE,
  CAN_DELETE, OPTIMAL_READ/WRITE_BUFFER_SIZE, FORMAT, RESOURCE_KEY}` only **[verified]**. The nearest capability is
  the command option `WPD_OPTION_OBJECT_RESOURCES_SEEK_ON_READ_SUPPORTED` (`GetCommandOptions`) and whether
  `GetSupportedCommands` lists `WPD_COMMAND_OBJECT_RESOURCES_SEEK`. The probe prints both, and
  `WPD_OPTION_OBJECT_RESOURCES_NO_INPUT_BUFFER_ON_READ`.
- **Fewer releases.** Keep the stream across other requests is impossible (an open stream makes the device busy).
  What's left is scheduling: make other requests wait for the window to finish and serve them in a batch, then
  continue; or, on a forward-only device, release only when the reopen cost (offset ÷ measured speed) is small next
  to the wait it saves, otherwise let the request wait up to a bound. Cuts the slowdown, doesn't remove it
  **[guess]**.
- **Second `IPortableDevice` connection** for browsing while one holds the stream: untested; likely the same busy
  device **[guess]**.

## Test plan (Windows laptop + Pixel 9 Pro XL)

Setup: phone unlocked, file-transfer mode, Explorer and Photos closed on the phone. Put a ≥1 GB video on it (a >4 GiB
file too, if one is handy). In a checkout of this branch:

```
cargo run --release -p mtp-rs --example wpd_partial_probe -- "DCIM/Camera/<big>.mp4"
cargo run --release -p mtp-rs --example wpd_partial_probe -- "DCIM/Camera/<big>.mp4" --chunk 1M --bench 256M
cargo run --release -p mtp-rs --example wpd_partial_probe -- "DCIM/Camera/<big>.mp4" --interleave
cargo run --release -p mtp-rs --example wpd_partial_probe -- "DCIM/Camera/<big>.mp4" --early-end
# >4 GiB file only: proves the 64-bit offset
cargo run --release -p mtp-rs --example wpd_partial_probe -- "<path>/<huge>.mp4" --offsets 0,4500M
```

Run `--interleave` and `--early-end` last and one at a time: they can wedge the phone (replug if the next run hangs).
Keep the full output of every run. What each section means:

1. **Driver capabilities**: MTP_EXT commands "listed" means the driver offers the passthrough. 0x95C1 among the vendor
   opcodes is a good sign. `OBJECT_RESOURCES_SEEK` listed, or `SEEK_ON_READ_SUPPORTED=true`, while `IStream::Seek`
   fails points at the raw-command alternative.
2. **Handle mapping check**: "CONFIRMED" means `o<hex>` is the MTP handle. "WRONG" or a failure means we need
   another mapping before anything else.
3. **IStream::Seek**: expect `E_NOTIMPL` (0x80004001) on the Pixel. "ok" means this phone seeks and #61 is about
   another device (the reporter's POCO X6 Pro).
4. **Per offset**: the decision point.
   - 0x95C1 response 0x2001 and "bytes match the stream" at every offset: partial reads work.
   - Capture each row's execute and total ms. Passthrough time should stay flat across offsets, while stream
     reopen time grows with the offset (its discard MiB/s says how fast).
   - `E_ACCESSDENIED` even after the IOCTL retry, or `E_NOTIMPL`: the driver or API blocks it. An HRESULT
     `0x8004_2xxx` or a response like 0x2005: the device refused (OperationNotSupported).
   - 0x101B: the same for the 32-bit op; a fallback for devices without 0x95C1.
5. **Raw resource commands**: SEEK ok and "matches stream bytes: yes" means a seekable path exists without the
   passthrough.
6. **Two windows with a listing between**: window 2 about as fast as window 1 is the #61 fix in miniature.
7. **Throughput**: compare passthrough MiB/s at each chunk size with "stream, one pass". Within ~10% is fine;
   much slower means windows need big chunks, or the passthrough only pays off after other calls.
8. **`--interleave`**: whether `GetValues` and a stream open run, fail busy, or block inside an open passthrough
   transfer, and whether a passthrough call works while a stream is open. Tells us how other WPD clients see us.
9. **`--early-end`**: END after one chunk answering, and all three health checks passing, means a cancel can stop
   mid-window. Anything else means cancel drains the window first.

Later: ask the #61 reporter to run the same probe on the POCO X6 Pro (Windows 11).

## Results (Pixel 9 Pro XL, Windows 10 Pro 22H2, 2026-11-08) [verified]

Two probe runs on a 1 GiB random file in `Download/`, the second with `--early-end`. Both runs agreed.

- **The driver lists the passthrough commands** (`EXECUTE_COMMAND_WITH_DATA_TO_READ`, `READ_DATA`, `END_DATA_TRANSFER`) and reports vendor opcodes 0x95C1–0x95C5. It doesn't list `OBJECT_RESOURCES_SEEK`.
- **Handle mapping is right**: object id `oEF` parses to 0xEF, and a passthrough `GetObjectInfo` on 0xEF returns the same name, size, and parent.
- **Seek is impossible on this stream**: `IStream::Seek` and the raw `WPD_COMMAND_OBJECT_RESOURCES_SEEK` both return `E_NOTIMPL` (0x80004001).
- **A 4 MiB window costs the same at any offset through the passthrough**, with both 0x95C1 and 0x101B. Times per window:
  - Offset 0: stream reopen 119–131 ms, passthrough 126–145 ms.
  - Offset 512 MiB: stream reopen 14.6 s (it discards 512 MiB at 35 MiB/s), passthrough 129–140 ms.
  - Offset 1020 MiB: stream reopen 28.6 s, passthrough 126–133 ms.
- **Every window's bytes match the stream.**
- **Throughput on the first 64 MiB**: one stream pass ran at 30 MiB/s. The passthrough in 8 MiB windows ran at 32.5–33.5 MiB/s, whatever the chunk size (driver-optimal 256 KiB, 1 MiB, or 4 MiB).
- **A listing between two passthrough windows took 2.9 ms**, and the next window ran at full speed. There's no stream to release.
- **An early `END_DATA_TRANSFER` after 256 KiB of a 16 MiB window failed with `ERROR_CANCELLED` (0x800704C7) after 313 ms**, but the session stayed healthy: the next `GetObjectInfo`, partial read, and stream read all worked. This is the driver cancelling the transfer itself, so on Android it may hit the same `io_cancel` race as our `CLASS_CANCEL` (#54). One run proves nothing about a race that wedged about one cancel in ten. Until a loop of early ENDs shows no wedge, cancel by reading the rest of the window (at most about one pacing target).
- **Not covered yet**: the reporter's POCO X6 Pro, Samsung, a camera under WPD, and files over 4 GiB.

## If it works: the shape of the fix

- `WpdBackend::can_read_partially_at(offset)`: true when the device advertises 0x95C1 (or 0x101B and offset ≤ u32)
  in its MTP `DeviceInfo` (passthrough 0x1001 once at open, or the vendor opcode list) and the object id parses.
- `download_window` and `read_range` on the worker: one passthrough transaction per window, streaming `READ_DATA`
  chunks over the existing channel. No kept stream, so nothing to release between windows; the kept-stream logic
  stays for held-open downloads and devices without partial reads.
- Cancel: stop forwarding, drain the window's remaining `READ_DATA`, `END_DATA_TRANSFER` (unless `--early-end` shows
  END alone is safe).
- Bounds: window ≤ u32::MAX by protocol; the pacer already caps it.
