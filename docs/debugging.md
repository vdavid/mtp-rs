# Debugging hub

Central guide for debugging `mtp-rs`, especially against physically-connected
hardware. Everything here is **needed only when you're actually debugging with a
real device** (issues like #12, #18) or capturing USB traffic. Skip it for
virtual-device / mock work; those paths never touch USB.

Two parts:

1. [Real-device debugging](#real-device-debugging-read-this-first): setup,
   recovery, and device gotchas. Read before you plug in a phone.
2. [USB capture and analysis](#usb-capture-and-analysis): recording and decoding
   the raw MTP conversation with Wireshark.

## Real-device debugging: read this first

### 1. Block ptpcamerad on macOS (run this yourself, keep it running)

macOS's `ptpcamerad` daemon grabs the PTP/MTP interface the instant a phone
connects, so `nusb` can't claim it and **every open times out** even though the
phone is connected, unlocked, and in File transfer mode. Agents: start this loop
in the background yourself at the start of a real-device session and leave it
running; don't make the user do it.

```sh
while :; do pgrep -f "[p]tpcamerad" >/dev/null && pkill -9 -f "[p]tpcamerad"; sleep 0.5; done
```

The `[p]` bracket stops the pattern from matching its own `pgrep`/`pkill`. A
stray `ptpcamerad` grab is the most common cause of "device present on USB but
`open device - Timeout`" on macOS.

### 2. Recover a wedged device: quiet reopens first, reset last

**Before anything else, check nothing else owns the interface.** A running `adb`
server holds the USB device, and a perfectly healthy phone then enumerates, lists
in `mtp-rs devices`, and times out on every open: the identical symptom to a
wedge. So is a stray `ptpcamerad` (step 1 above). This masquerade cost hours
during the 2026-07 investigation.

Symptoms of a real wedge, on a device that worked moments ago:

- `open device - Timeout`,
- `expected Response container type (3), got N` or a transaction-ID mismatch,
- every operation timing out after one bad operation,
- or, on a Pixel, an operation that simply **hangs and never returns**, with no
  error at all (verified on a Pixel 9 Pro XL, macOS/nusb, 2026-07-20). Detection
  logic that only matches `Error::DeviceReset` misses this case entirely.

The recovery order:

1. Drop the `MtpDevice` and every `Storage` handle (holding one keeps the USB
   interface claimed).
2. Wait a few seconds **quiet**, with no USB traffic at all.
3. Reopen with idle-spaced retries, several of them. Don't hammer close/open in a
   tight loop; that keeps the device busy and re-wedges it into a hard `Timeout`.
   A Pixel's wedge cleared on a fresh open with no reset at all (verified on a
   Pixel 9 Pro XL, macOS/nusb, 2026-07-20).
4. Only when all of that has failed: `mtp-rs reset` (CLI) /
   `MtpDevice::reset_by_serial()` / `PtpDevice::reset_device()` (SIC
   `DEVICE_RESET`, 0x66), then repeat steps 2 and 3.

**Why the reset is last: on Android it can break MTP until a physical replug.**
Sent to a *healthy* Pixel 9 Pro XL, it killed the phone's MTP function: Android's
`MtpServer` lost its FunctionFS endpoint read (`ECANCELED`, then `EPIPE`) and
never re-armed, while the USB device controller stayed `configured`, so the phone
kept enumerating and answered nothing. Ten spaced reopens over about 100 s all
timed out; only an unplug and replug fixed it (verified on a Pixel 9 Pro XL,
macOS/nusb + `adb logcat`, 2026-07-21). Full evidence:
[notes/android-wedges-and-the-reset-kill-switch.md](notes/android-wedges-and-the-reset-kill-switch.md).

The reset stays the right tool for a device that's **already** unreachable, and
for the cross-process poison it exists for: it opens without a PTP session, so it
works precisely when `MtpDevice::open` can't. On a Galaxy S23 Ultra (#18) it
looked like the cure: after a cancel wedged the session,
`cargo run -p mtp-rs-cli -- reset` printed `Reset OK, device responding: SM-S918B`
and listing worked immediately, and a later run went reset, then reopens returning
`Timeout`, then `SessionAlreadyOpen`, then success (verified on SM-S918B,
macOS/nusb, 2026-07-20). But the control was never run, so it's unknown whether
spaced reopens alone would have done it. Don't read that as proof the reset helps.

A fully USB-stuck device (some PTP cameras, #12) needs a physical replug either
way.

### 3. Fail fast on a wedged or absent device

Integration tests read the open timeout from `MTP_TEST_TIMEOUT_SECS` (default 30,
so CI and real-device runs are unchanged). Export `MTP_TEST_TIMEOUT_SECS=2` while
iterating so a wedged or absent device **skips in ~2s instead of stalling 30s per
op** (otherwise the destructive-first suite hangs for minutes on an unopenable
device). Healthy operations here finish well under a second, so 2s is safe.

### 4. Android gotchas (Samsung and Pixel alike)

- **USB mode resets on reconnect.** Replugging a Samsung reverts it to "charging
  / no data transfer" and re-arms the "Allow access?" prompt. Re-select File
  transfer after every replug, or opens time out.
- **Momentary zero storages after reconnect.** Right after a reconnect the device
  can briefly report zero storages, so an immediate `storages()[0]` panics. Give
  it a beat and retry.
- **Interrupting an in-flight bulk read can wedge the session** (#18), and this is
  **not Samsung-specific**: a Pixel 9 Pro XL wedges from the same trigger (clean
  A/B, same binary minutes apart, no-drop run fine, drop run hung; verified on a
  Pixel 9 Pro XL, macOS/nusb, 2026-07-20). Cancelling or abandoning a transfer
  leaves the transaction unclosed and the session desynced. **Size is not the
  trigger**: a 36-byte file wedged it (verified on a Galaxy S23 Ultra SM-S918B,
  macOS/nusb, 2026-07-20), so don't dismiss a report because the file was small.
- **The signature differs by device, and that's a trap for consumers.** On a
  Samsung the next operation surfaces `Error::DeviceReset` (the library detects
  the wedge, resets the transport to un-stick it, and reports it; it does not
  reopen). On a Pixel the next operation just **hangs**, with no error to match
  on. Code watching only for `DeviceReset` never notices the Pixel case, so wrap
  operations in a timeout too. Recovery for both: see step 2 above (quiet, spaced
  reopens; the reset last).
- **One flavor doesn't recover in software**: a dropped **held-open streaming**
  `GetObject` (`FileDownload`) future needed a physical replug on the S23 (plain
  reopen and transport reset both failed), while a dropped **windowed**
  `GetPartialObject64` future recovered via the reset-plus-spaced-retries path
  (both verified 2026-07-20). So `download_windowed` doesn't dodge the wedge, only
  the need to cancel; prefer it because its wedge is the recoverable one.

### 5. Capture diagnostics for a bug report

Two purpose-built ways to get "what did the device actually do" without a
Wireshark trace. Ask a reporter for these first; they usually pinpoint the fault.

- **`mtp-rs doctor --probe-cancel`**: prints device identity, capabilities, and
  storages, then runs the cancel-health probe (download a file, cancel
  mid-stream) and classifies the result: `healthy`, `wedged_recovered`
  (the #18 signature: the library reset the device and returned `DeviceReset`),
  or `errored`. Add `--json` for a machine-readable bundle. Plain `doctor` (no
  flag) stays passive; `--probe-cancel` transfers data and can briefly wedge a
  device (the library recovers it).
  - The probe searches **below** the root for the file, breadth-first, bounded to
    48 folders and three levels. An Android root holds only directories, so a
    root-only look skipped the probe on exactly the phones #18 is about (verified
    on a Pixel 9 Pro XL: 17 directories, zero files).
  - It prefers a file of 100 KB-10 MB but takes **any** file rather than
    skipping, because size doesn't drive the wedge.
  - `--probe-path /DCIM/Camera/IMG_0001.jpg` pins the file and implies
    `--probe-cancel`. Use it when the search picks a file you'd rather leave
    alone, or when it finds none.
- **Protocol trace**: the CLI emits the library's `tracing` events to stderr.
  - `mtp-rs --trace <cmd>` → the cancel/reset path and session recovery at debug
    level (the #18-relevant events).
  - `RUST_LOG=mtp_rs=trace mtp-rs <cmd>` → adds per-operation detail
    (`execute*: op=… -> …`), so you see the exact sequence and where it stalls.
  - Stderr-only, so `--json`/piped stdout stays clean.
- **For library consumers** (not the CLI): build `mtp-rs` with the `tracing`
  feature and install any `tracing` subscriber. Off by default — no dependency,
  no cost — so a plain build stays lean. The events live on the transaction and
  cancel/reset paths (`crates/mtp-rs/src/trace.rs` is the shim).

### Reproducing device wedges without hardware

The virtual device can model the #18 cancel wedge for regression tests, two
one-shot hooks for the two ways a consumer reaches `Error::DeviceReset`:

- `force_cancel_wedge(serial)`: the next `cancel_transfer` returns
  `DeviceReset`, so a mid-stream `cancel()` surfaces it. See
  `cancel_wedge_surfaces_device_reset` in `transport/virtual_device/mod.rs`.
- `force_operation_wedge(serial)`: the next **operation** returns `DeviceReset`,
  for a consumer that never calls `cancel()` and only meets the error through
  `recover_if_needed`'s drain after a dropped future. See
  `operation_wedge_surfaces_device_reset_without_a_cancel`, and
  `a_wedged_root_listing_reports_the_reset_it_hit` for the root-listing case.

Neither models the aftermath: a real device's session is dead until a
spaced-retry reopen, the virtual one is healthy on the next call.

### Other integration-test env knobs

The header of `crates/mtp-rs/tests/integration.rs` documents the rest:
`MTP_TEST_FOLDER` (writable folder override), `MTP_TEST_READFILE` (pin a file,
skip the search), `MTP_RUN_SLOW_TESTS`, and `MTP_RUN_DROP_RECOVERY` (the opt-in
mid-stream-drop recovery test, which can wedge a device until a replug).

## USB capture and analysis

This section covers how to capture and analyze USB traffic for debugging MTP
issues.

### What you're capturing

MTP runs over USB bulk transfers. When you connect your phone and browse files, the conversation looks like:

```
Your Computer                          Phone
     │                                   │
     │──── "Open session please" ───────▶│
     │◀─── "OK, session open" ───────────│
     │                                   │
     │──── "List your storages" ────────▶│
     │◀─── "Internal: 64GB, SD: 32GB" ───│
     │                                   │
     │──── "List files in root" ────────▶│
     │◀─── "DCIM/, Download/, ..." ──────│
     │                                   │
     └───────────────────────────────────┘
```

You're recording both sides of this conversation as raw bytes.

### Tools

#### Wireshark (recommended)

- Works on Linux, macOS, Windows
- Visual interface to see packets in real-time
- Can export to multiple formats
- On Linux: needs `usbmon` kernel module
- On macOS: needs additional setup but works
- On Windows: needs USBPcap

#### usbmon + tcpdump (Linux)

- Lower level, text-based
- Good for scripting
- Linux only

### Capture process

#### 1. Preparation

- Close all file managers and apps that auto-mount MTP
- On Linux: stop `gvfs-mtp-volume-monitor` or similar
- You want a clean slate - no background MTP traffic

#### 2. Start capture

- Open Wireshark
- Select your USB bus (the one your phone will connect to)
- Start recording

#### 3. Connect phone

- Plug in USB cable
- Phone shows "USB connected" notification
- Select "File Transfer / MTP" mode on phone
- You'll see initial handshake packets appear in Wireshark

#### 4. Perform specific operations

Do each operation **deliberately and one at a time** so you can label them later:

| Operation               | What to do                      | What it captures                     |
|-------------------------|---------------------------------|--------------------------------------|
| **Device detection**    | Just connect                    | GetDeviceInfo                        |
| **Open session**        | Let file manager connect        | OpenSession                          |
| **List storages**       | Open the device in file browser | GetStorageIDs, GetStorageInfo        |
| **List root folder**    | Click into Internal Storage     | GetObjectHandles, GetObjectInfo (×N) |
| **Navigate to folder**  | Click into DCIM                 | GetObjectHandles for that folder     |
| **Read file metadata**  | Select a file (don't open)      | GetObjectInfo                        |
| **Download small file** | Copy a small file to PC         | GetObject                            |
| **Upload small file**   | Copy a small text file to phone | SendObjectInfo, SendObject           |
| **Delete file**         | Delete that test file           | DeleteObject                         |
| **Close session**       | Safely eject / disconnect       | CloseSession                         |

#### 5. Stop capture

- Disconnect phone cleanly (eject first)
- Stop Wireshark recording
- Save the raw capture file (.pcapng)

### Reading raw captures

Wireshark shows you something like:

```
No.  Time     Source  Dest    Protocol  Info
1    0.000    host    1.2.1   USB       URB_BULK out
2    0.005    1.2.1   host    USB       URB_BULK in
3    0.006    host    1.2.1   USB       URB_BULK out
...
```

Each packet has raw bytes. For MTP, you'll see the container structure:

```
Frame 42: URB_BULK out (host → device)
  Raw: 10 00 00 00 01 00 02 10 01 00 00 00 01 00 00 00
       └─ length ─┘ └type┘ └code┘ └─ trans_id ─┘ └param1─┘

  Decoded: Command Container
           Length: 16 bytes
           Type: Command (0x0001)
           Code: OpenSession (0x1002)
           Transaction ID: 1
           Param1: 1 (session ID)
```

### Processing captures

#### Group into request/response pairs

Each MTP transaction is:

```
Command (out) → [Data (in/out)] → Response (in)
```

Group these by transaction ID.

#### Extract and label

For each transaction, save:

- The command bytes
- Any data bytes
- The response bytes
- A human label ("GetStorageIDs", "ListRootFolder", etc.)

### Using captures for test fixtures

After processing, you'd have something like:

```
fixtures/
├── pixel6_session.json        # Full session from connect to disconnect
├── operations/
│   ├── open_session.json      # Just OpenSession request/response
│   ├── get_storage_ids.json   # GetStorageIDs
│   └── download_file.json     # GetObject for a specific file
└── structures/
    ├── device_info.bin        # Raw DeviceInfo response payload
    └── object_info.bin        # Raw ObjectInfo response payload
```

Each JSON file might look like:

```json
{
  "description": "Open MTP session",
  "device": "Google Pixel 6",
  "transaction": {
    "command": {
      "hex": "10000000010002100100000001000000",
      "decoded": {
        "length": 16,
        "type": "Command",
        "code": "OpenSession",
        "transaction_id": 1,
        "params": [1]
      }
    },
    "response": {
      "hex": "0c00000003000120010000",
      "decoded": {
        "length": 12,
        "type": "Response",
        "code": "OK",
        "transaction_id": 1
      }
    }
  }
}
```

### Safety notes

| Concern                  | Risk level   | Mitigation                                                |
|--------------------------|--------------|-----------------------------------------------------------|
| Capturing damages phone  | **None**     | You're just observing USB traffic                         |
| Uploading corrupts data  | **Very low** | Only upload a test file you create, then delete it        |
| Private data in captures | **Medium**   | Filenames, folder structure visible - don't share raw captures publicly |
| Phone left in bad state  | **Very low** | Always cleanly eject before disconnecting                 |

### Recommended capture sessions

#### Session 1: Basic discovery (read-only, safest)

1. Connect
2. Let it enumerate storages
3. Browse to DCIM
4. Browse to a subfolder
5. Disconnect cleanly

#### Session 2: File operations (minimal writes)

1. Connect
2. Navigate to Download folder
3. Copy a tiny test.txt (10 bytes) TO the phone
4. Read it back
5. Delete it
6. Disconnect

#### Session 3: Edge cases (if needed)

- Large file transfer (to test chunking)
- File with unicode name
- Deep folder navigation
