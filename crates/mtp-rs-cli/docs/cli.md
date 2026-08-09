# mtp-rs CLI

`mtp-rs` includes a universal MTP file-transfer CLI. It is intentionally
device-neutral: Garmin watches, Android phones, Kindles, media players, and
cameras that expose MTP storage all use the same file commands.

## Installation

Install the binary from crates.io:

```sh
cargo install mtp-rs-cli
```

The installed binary is called `mtp-rs`.

If you are working from a checkout:

```sh
cargo run -p mtp-rs-cli -- devices
```

## Quick Start

```sh
# Show visible devices.
mtp-rs devices

# Inspect a specific device and its storages.
mtp-rs info --device SERIAL

# List files.
mtp-rs ls /
mtp-rs ls /Music --recursive

# Upload and download files.
mtp-rs put ./song.mp3 /Music/song.mp3 --replace
mtp-rs get /Music/song.mp3 ./song.mp3

# Create and remove remote objects.
mtp-rs mkdir /Upload
mtp-rs rm /Upload/old.bin --yes

# Rename, move, and copy remote objects.
mtp-rs rename /Upload/old.bin new.bin
mtp-rs mv /Upload/new.bin /Archive/new.bin
mtp-rs cp /Archive/new.bin /Archive/new-copy.bin

# Diagnose common MTP access problems.
mtp-rs doctor
```

## Global Options

| Option | Description |
| --- | --- |
| `--device SERIAL` | Select a physical device by serial number. |
| `--location HEX_OR_DECIMAL` | Select the USB location shown by `devices`. |
| `--storage INDEX_OR_ID` | Select a storage by zero-based index or storage ID. |
| `--known VID:PID` | Include a non-standard MTP device in discovery. Can be repeated. |
| `--timeout SECONDS` | Set the USB transfer timeout. Defaults to 30 seconds. |
| `--json` | Emit machine-readable JSON for every command. Transfer progress still goes to stderr. |
| `--verbose` | Include lower-level protocol or transport error detail. |
| `--trace` | Print device-protocol diagnostics to stderr (the cancel/reset path, session recovery), for bug reports. Equivalent to `RUST_LOG=mtp_rs=debug`; a set `RUST_LOG` takes precedence, so `RUST_LOG=mtp_rs=trace` adds per-operation detail. |

`--device` and `--location` are mutually exclusive. If neither is passed,
commands open the only visible MTP device. If multiple devices are visible,
pass one of them explicitly.

Mutating commands select the first storage only when the device exposes a
single storage. If a device exposes multiple storages, pass `--storage` for
`put`, `mkdir`, `rm`, `rename`, `mv`, and `cp`.

## Remote Paths

Remote paths are POSIX-like and absolute:

```text
/
/Music/song.mp3
/Download/report.pdf
/GARMIN/APPS/MyApp.prg
```

Path matching is case-sensitive in v1 because MTP devices disagree about
case behavior. Components containing `/`, `\`, null bytes, `.`, or `..` are
rejected before sending an MTP request.

For destination paths:

- `DEST/` means “use the source or local filename inside `DEST`”.
- If `DEST` exists and is a folder, the command writes inside that folder.
- If a destination file exists, pass `--replace` where supported.

## Commands

### `devices`

List visible MTP devices.

```sh
mtp-rs devices
mtp-rs devices --json
mtp-rs devices --known 091e:0003
```

The output includes VID/PID, manufacturer, product, serial number, USB
location, negotiated link speed when the OS reports it, and `match_reason`.
`match_reason` explains why the device was accepted as an MTP candidate:
`standard_class`, `interface_string`, `known_vid_pid`, or
`opened_descriptor_scan`.

### `info`

Open a device and show device metadata plus storages.

```sh
mtp-rs info
mtp-rs info --device SERIAL
mtp-rs info --location 0xffff000000000000 --json
```

Use this before mutating commands when the device has multiple storages.

### `ls`

List a remote folder.

```sh
mtp-rs ls /
mtp-rs ls /Music
mtp-rs ls /Music --recursive
mtp-rs ls / --storage 0 --json
```

`ls` requires the path to resolve to a folder.

Some devices list an object and then refuse to describe it. `ls` shows the rest of the folder rather
than failing, and says what it left out on stderr:

```
warning: 1 object could not be read and was left out:
  handle=20 GeneralError
```

The warning goes to stderr in both plain and `--json` modes, so stdout stays clean for pipes. In
`--json` the same objects appear in a `skipped` array, which is always present (empty when
everything was readable) so a script can read one field without checking whether the key exists:

```json
{ "path": "/", "recursive": false, "objects": [], "skipped": [{ "handle": 20, "error": "GeneralError" }] }
```

A folder where *every* object failed is an error, not an empty listing.

### `put`

Upload a local file to a remote path.

```sh
mtp-rs put ./song.mp3 /Music/song.mp3
mtp-rs put ./song.mp3 /Music/ --replace
mtp-rs put ./MyApp.prg /GARMIN/APPS/MyApp.prg --replace --verify
```

Behavior:

- If `REMOTE_PATH` ends with `/`, the local filename is used.
- If `REMOTE_PATH` resolves to an existing folder, the file is uploaded inside it.
- If the destination file exists, `--replace` deletes it before upload.
- `--verify` downloads the uploaded object back and compares bytes. This is
  intentionally explicit because it doubles transfer traffic.

Uploads are streamed; large files are not buffered in memory.

### `get`

Download a remote file to a local path.

```sh
mtp-rs get /Music/song.mp3 ./song.mp3
mtp-rs get /Music/song.mp3 ./song.mp3 --replace
```

`get --replace` writes to a temporary file first and replaces the destination
only after the download succeeds, so an interrupted transfer does not destroy
the existing local file.

### `mkdir`

Create one remote folder under an existing parent.

```sh
mtp-rs mkdir /Upload
mtp-rs mkdir /GARMIN/APPS
```

This does not create missing intermediate folders.

### `rm`

Delete a remote object.

```sh
mtp-rs rm /Upload/old.bin --yes
```

`--yes` is required. Folder deletion depends on what the device supports; many
devices reject non-empty folder deletes.

### `rename`

Rename a remote object in place.

```sh
mtp-rs rename /Upload/old.bin new.bin
```

`new.bin` must be a filename, not a path. The device must advertise rename
support.

### `mv`

Move a remote object to another folder or path.

```sh
mtp-rs mv /Upload/file.bin /Archive/file.bin
mtp-rs mv /Upload/file.bin /Archive/
mtp-rs mv /Upload/file.bin /Archive/new-name.bin
```

If the destination changes the filename, the device must support rename.
If the destination file exists, pass `--replace`.

### `cp`

Copy a remote object to another folder or path.

```sh
mtp-rs cp /Upload/file.bin /Archive/file.bin
mtp-rs cp /Upload/file.bin /Archive/
mtp-rs cp /Upload/file.bin /Archive/file-copy.bin
```

If the destination changes the filename, the device must support rename.
If the destination file exists, pass `--replace`.

### `doctor`

Diagnose common discovery and access problems.

```sh
mtp-rs doctor
mtp-rs doctor --device SERIAL --json
mtp-rs doctor --probe-cancel        # also run the cancel-health check
mtp-rs doctor --probe-path /DCIM/Camera/IMG_0001.jpg   # probe that exact file
```

`doctor` checks visible devices, open behavior, capabilities, storages, root
listing, and common writable-folder hints such as `Download`, `Documents`,
`Music`, and `GARMIN`. In JSON mode it keeps the visible device rows, including
`match_reason`, even when opening the selected device fails.

`--probe-cancel` additionally downloads a file, cancels it mid-stream, and
reports whether the session stayed `healthy`, was `wedged_recovered` (the device
wedged on the cancel and the library reset it to recover; see issue #18), or
`errored`. It's read-only but transfers data and can briefly wedge a device, so
it's off by default; use it when investigating a mid-transfer freeze. Attach the
`--probe-cancel --json` output to a bug report.

To find a file, the probe walks breadth-first from the storage root, bounded to
48 folders and three levels deep, preferring a 100 KB-10 MB file but taking any
file rather than skipping (a 36-byte file is enough to wedge a phone). An
Android root holds only directories, so looking there alone finds nothing. When
it does skip, the message says how far it searched.

`--probe-path PATH` pins the file to probe and implies `--probe-cancel`. Reach
for it when the search picks a file you'd rather leave alone, or when the device
keeps its files somewhere the bounded search misses.

### `reset`

Last-resort reset of a stuck device's USB transport state. No PTP session
needed, so it works when every other command fails.

```sh
mtp-rs reset
mtp-rs reset --device SERIAL --json
```

**Try waiting and retrying first.** A device that stops responding after an
interrupted transfer often clears on its own: leave it idle for a few seconds
with no USB traffic, then run your command again, a few times with gaps between
attempts. On a Pixel the wedge cleared on a fresh open with no reset at all
(verified on a Pixel 9 Pro XL, macOS/nusb, 2026-07-20).

**On Android, `reset` can break MTP until you replug the phone.** Sent to a
healthy Pixel 9 Pro XL, it killed the phone's MTP function outright: Android's
`MtpServer` lost its endpoint and never came back, while the phone kept
enumerating over USB and answering nothing. Ten spaced reopens over about 100 s
all timed out, and only a physical unplug and replug fixed it (verified on a
Pixel 9 Pro XL, macOS/nusb, 2026-07-21).

So reach for `reset` when the device is **already** unreachable and retrying has
failed, where you can't make things much worse. Typical symptoms are every
command failing with "Transaction ID mismatch" or "expected Response container
type" errors. `reset` sends the USB Still Image Class Device Reset request (the
same recovery Windows uses), clears halted endpoints, and drains stale data. It
then verifies the device responds again with a session-less GetDeviceInfo.

Works on real USB devices only (virtual devices have no USB transport).
Some devices with hard-wedged firmware still need a power cycle; `reset`
covers everything short of that.

Background and the full evidence:
[android-wedges-and-the-reset-kill-switch.md](../../../docs/notes/android-wedges-and-the-reset-kill-switch.md).

## JSON Output

Pass `--json` to any command for machine-readable output:

```sh
mtp-rs devices --json
mtp-rs info --json
mtp-rs put ./song.mp3 /Music/song.mp3 --json
```

JSON is written to stdout. Transfer progress is written to stderr, so stdout
remains parseable JSON during `put`, `get`, and `put --verify`.

For mutating commands, JSON includes an `operation` field and relevant paths,
handles, filenames, byte counts, and booleans such as `verified`, `renamed`,
or `replaced`. `replaced` means an existing visible destination object was
actually replaced.

## Exit Codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | Other error. |
| `2` | No device found. |
| `3` | Ambiguous device or storage selection. |
| `4` | Access denied, read-only storage, or exclusive access. |
| `5` | Remote path error. |
| `6` | Transfer failed. |
| `7` | Verification failed. |

Use `--verbose` when an automation needs lower-level protocol context in
stderr.

## Device Workflows

### Android

Android devices usually reject uploads to the storage root. Upload into an
existing folder such as `Download`:

```sh
mtp-rs put ./report.pdf /Download/report.pdf --replace
mtp-rs get /Download/report.pdf ./report.pdf
```

Make sure the phone is unlocked and USB mode is set to file transfer.

### Kindle

Copy documents into the documents folder:

```sh
mtp-rs put ./book.epub /documents/book.epub --replace
mtp-rs ls /documents
```

### Garmin

Connect IQ sideloading is a normal file upload:

```sh
mtp-rs put ./MyApp.prg /GARMIN/APPS/MyApp.prg --replace --verify
```

Garmin remains just one workflow; the CLI does not include Garmin-specific
commands.

### Generic Backup

Download files by path after listing folders:

```sh
mtp-rs ls /DCIM --recursive
mtp-rs get /DCIM/Camera/IMG_0001.jpg ./IMG_0001.jpg
```

## Troubleshooting

### No Device Found

Check that the device is connected, unlocked, and in MTP/file-transfer mode.
Some devices expose different USB modes for charging, PTP, tethering, and MTP.

### Multiple Devices

Run:

```sh
mtp-rs devices
```

Then repeat the command with `--device SERIAL` or `--location LOCATION`.

### Multiple Storages

Run:

```sh
mtp-rs info
```

Then pass `--storage INDEX_OR_ID` to mutating commands.

### macOS Exclusive Access

macOS may let Photos, `ptpcamerad`, Android File Transfer, Garmin Express, or
another file manager claim the USB device. Close those applications and retry.
The CLI detects common exclusive-access errors and prints a practical hint.

### macOS USB Authorization

On recent macOS versions, IOKit can also deny USB user-client access before the
device is opened. This is different from another app owning the interface: the
device is visible in `mtp-rs devices`, but `info`, `ls`, or `doctor` fail while
creating the IOKit PlugInInterface. Run the CLI from Terminal or iTerm with the
Mac unlocked and the accessory allowed. GUI helpers launched as normal `.app`
bundles can have different USB authorization than background agents or IDE task
runners.

### Linux Permissions

You may need udev rules to access USB devices without root. See the platform
notes in the main [README](../README.md#linux).

### Timeout Or Disconnect

Use a better cable or port, unlock the device, avoid sleep/lock during transfer,
and increase timeout for large files:

```sh
mtp-rs --timeout 120 put ./large-video.mp4 /Movies/large-video.mp4
```

### Device Stuck After An Interrupted Transfer

When a transfer or listing gets interrupted (Ctrl+C, crash, sleep), some
devices keep waiting for the rest of the transaction. Symptoms: every command
fails with "Transaction ID mismatch" or "expected Response container type"
errors, or on a phone, hangs with no error at all.

Recover in this order:

1. Leave the device idle for a few seconds, with nothing talking to it.
2. Run your command again. Repeat a few times with gaps between attempts, rather
   than in a tight loop: hammering keeps the device busy and re-wedges it.
3. Only when that has failed, run [`reset`](#reset). On Android it can break MTP
   until you physically replug the phone, so it's the last step, not the first.

```sh
mtp-rs reset
```

If the device is an Android phone and you have `adb` running against it, stop the
adb server first. It holds the USB device and produces the identical symptom on a
perfectly healthy phone.
