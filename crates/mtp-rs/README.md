# mtp-rs

[![Crates.io](https://img.shields.io/crates/v/mtp-rs)](https://crates.io/crates/mtp-rs)
[![docs.rs](https://img.shields.io/docsrs/mtp-rs)](https://docs.rs/mtp-rs)
[![CI](https://github.com/vdavid/mtp-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/vdavid/mtp-rs/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/mtp-rs)](LICENSE-MIT)
[![MSRV](https://img.shields.io/badge/MSRV-1.85-blue)](https://blog.rust-lang.org/2025/02/20/Rust-1.85.0.html)

A pure-Rust, async MTP/PTP library.
No C dependencies, consistently faster than libmtp (up to 4x for large transfers), and way more predictable.

Talk to Android phones, e-book readers incl. Kindle, and digital cameras over USB.
No `libmtp`, no `libusb`, no FFI, just async Rust built on [`nusb`](https://crates.io/crates/nusb).

**Why this matters:**

- Cross-compile without system lib headaches
- No `pkg-config`, no `-sys` crates, no `build.rs` surprises
- Works anywhere Rust compiles (including `musl` and cross-compilation targets)
- Fully async and runtime-agnostic
- [Virtual device mode](#virtual-device-testing-without-hardware) for testing without a USB device plugged in

Want a ready-made command-line tool on top of this library? See [`mtp-rs-cli`](https://crates.io/crates/mtp-rs-cli).

## What it does

- Connect to devices over USB
- List, download, upload, delete, move, copy, and rename files
- Create, delete, and rename folders
- Stream large file downloads and uploads with continued progress indication
- Listen for device events (file added, storage removed, etc.)
- See free space
- Report the negotiated USB link speed per device (USB 1.0 low through USB 3.2 Gen 2) so you can warn users when a fast device is throttled by a slow cable or port
- Also exposes a lower-level interface for PTP, so it can be used for cameras too.

## What it doesn't do

- MTPZ (the DRM extension some old devices used)
- Playlists, tracks, albums, and custom operations
- Vendor-specific extensions
- Legacy Android device quirks (pre-5.0 devices)

We intentionally didn't want to support these because they're rarely needed now, and it'd be a nightmare to test.
[libmtp](https://github.com/libmtp/libmtp/) has an impressive collection of device quirks, but it's LGPL-1.1 licensed,
and I wanted to do MIT/Apache-2.0 for broader access. So copying that code was also not an option.

## Quick start

A simple test would be this:

```rust
use mtp_rs::mtp::MtpDevice;

#[tokio::main]
async fn main() -> Result<(), mtp_rs::Error> {
    // Connect to the first MTP device
    let device = MtpDevice::open_first().await?;

    println!("Connected to {} {}",
             device.device_info().manufacturer,
             device.device_info().model);

    // List storages (internal storage, SD card, etc.)
    for storage in device.storages().await? {
        println!("{}: {:.2} GB free",
                 storage.info().description,
                 storage.info().free_space_bytes as f64 / 1e9);

        // List files in root
        for file in storage.list_objects(None).await? {
            let icon = if file.is_folder() { "📁" } else { "📄" };
            println!("  {} {}", icon, file.filename);
        }
    }

    Ok(())
}
```

## Cancellation

Long operations on large folders (Android `/DCIM/Camera` with 1k+ photos, recursive deletes) are made up of many small per-object USB roundtrips. Pass a [`CancelToken`](https://docs.rs/mtp-rs/latest/mtp_rs/cancel/struct.CancelToken.html) to bail mid-loop within one roundtrip's latency:

```rust
use mtp_rs::{CancelToken, mtp::MtpDevice};

#[tokio::main]
async fn main() -> Result<(), mtp_rs::Error> {
    let device = MtpDevice::open_first().await?;
    let storage = &device.storages().await?[0];

    let cancel = CancelToken::new();

    // Fire the cancel from another task.
    let cancel_for_task = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        cancel_for_task.cancel();
    });

    match storage.list_objects_with_cancel(None, Some(&cancel)).await {
        Ok(objects) => println!("Listed {} objects", objects.len()),
        Err(mtp_rs::Error::Cancelled) => println!("Cancelled mid-listing"),
        Err(e) => return Err(e),
    }
    Ok(())
}
```

`CancelToken` is `Arc`-backed (cheap to clone), `Send + Sync`, and one-way (no reset, make a fresh token per logical operation). Streaming downloads use a separate mechanism: see [`FileDownload::cancel`](https://docs.rs/mtp-rs/latest/mtp_rs/mtp/struct.FileDownload.html#method.cancel) for USB SIC class-cancel.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
mtp-rs = "0.17"
```

You'll also need an async runtime. The library is runtime-agnostic, but [tokio](https://github.com/tokio-rs/tokio) is
the most common choice:

```toml
[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

## Platform notes

### Linux

You may need udev rules to access USB devices without root. Create `/etc/udev/rules.d/99-mtp.rules`:

```
SUBSYSTEM=="usb", ATTR{idVendor}=="*",  MODE="0666"
```

Then run `sudo udevadm control --reload-rules`.

### macOS

It's a bit of a nightmare because macOS's built-in `ptpcamerad` daemon automatically claims MTP/PTP devices right on
connection, blocking other apps. This sucks because it it NOT `MTP`, just `PTP`, so Android phones, Kindles, etc.
won't be able to sync files through it, and at the same time, other apps (like potentially yours if you're looking at
this) will be unable to access the device. 🤯

One more potential offender is [Android File Transfer](https://www.android-file-transfer-mac.com/): If installed, it
spawns a process that also grabs devices. You must quit it before trying to connect to an MTP device using this (or,
honestly, any) library.

**Workarounds:**

1. **Kill loop**: Run this in Terminal while using your app:
   ```bash
   while true; do pkill -9 ptpcamerad 2>/dev/null; sleep 1; done
   ```

2. **Disable `ptpcamerad`**: Persistent, but may break Photos.app:

   ```bash
   sudo launchctl disable system/com.apple.ptpcamerad
   ```

**Other tips for app developers:**

- This library provides `Error::is_exclusive_access()`. Use this to detect this condition and guide users to apply
  one of the workarounds above.
- Query IORegistry for `UsbExclusiveOwner` to show which process (pid, name) holds the device for even more helpful info
- App Store sandboxed apps cannot kill processes. If your app is such, then provide the command for users to run
  manually.
  If your app isn't in the App Store, then you're in a better position and may be able to use the workarounds, BUT
  it's a bit murky territory with Apple.
- See [Cmdr](https://github.com/vdavid/cmdr) and [Commander One](https://mac.eltima.com/file-manager.html) for UX
  inspiration on handling this gracefully.

### Windows

Works out of the box via the native Windows Portable Devices (WPD) COM backend — no driver install
(no Zadig), no extra dependencies. mtp-rs uses Windows' own MTP stack, so the high-level `mtp::` API
auto-selects the WPD backend and you write the same code as on Linux and macOS. Verified on a
Pixel 9 Pro XL.

The low-level `ptp::` USB API is WinUSB-only on Windows: it reaches a device only when that device's
interface is bound to WinUSB (a camera, or a device you've bound with Zadig). Phones are bound to the
WPD driver, so reach them through `mtp::` (WPD), not `ptp::`.

## Examples

These might come in handy:

### Download a file

```rust
let storage = & device.storages().await?[0];

// Find a file
let files = storage.list_objects(None).await?;
let photo = files.iter().find( | f| f.filename == "photo.jpg").unwrap();

// Download it
let data = storage.download_to_vec(photo.handle).await?;
std::fs::write("photo.jpg", data) ?;
```

### Upload a file

```rust
use mtp_rs::mtp::NewObjectInfo;
use bytes::Bytes;

let content = std::fs::read("document.pdf") ?;
let info = NewObjectInfo::file("document.pdf", content.len() as u64);

let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(content))]);
let handle = storage.upload(None, info, Box::pin(stream)).await?;

println!("Uploaded with handle {:?}", handle);
```

Uploads are two-phase: the device creates the object, then receives the data. If
the data phase fails or is cancelled, the device is left holding a partial
object. `upload()` returns an `UploadError` whose `partial` field carries that
object's handle so you decide what to do. The library never auto-deletes it,
which keeps resume (retrying the data phase against the same handle) possible:

```rust
match storage.upload(None, info, Box::pin(stream)).await {
    Ok(handle) => println!("Uploaded with handle {:?}", handle),
    Err(e) => {
        if let Some(partial) = e.partial {
            // Discard the corrupt artifact (or retry the data phase to resume).
            storage.delete(partial).await?;
        }
        return Err(e.source); // `UploadError: Into<Error>` keeps `?` ergonomic.
    }
}
```

### Download with progress

`download_windowed` is the recommended way to read a file: it fetches bounded windows and frees the device's single PTP session between each, so other operations (listings, navigation) stay responsive during a long download.

```rust
let mut download = storage.download_windowed_default(file.handle).await?;
println!("Downloading {} bytes...", download.size());

while let Some(window) = download.next_window().await {
let bytes = window ?;
// Process bytes... (do other device work between windows if you like)
println ! ("{:.1}%", download.offset() as f64 / download.size() as f64 * 100.0);
}
```

For raw throughput when nothing else needs the device during the read, `download(handle, ByteRange::Full)` reads the whole file in one continuous transfer instead. It holds the PTP session for the entire download, so the device can't service other operations meanwhile, and it yields chunks as they arrive:

```rust
use mtp_rs::mtp::ByteRange;

let mut download = storage.download(file.handle, ByteRange::Full).await?;
while let Some(chunk) = download.next_chunk().await {
    file_on_disk.write_all( & chunk ?) ?;
}
```

Peak memory per transfer is one 64 KiB USB read, whatever the file's size. Pass `ByteRange::From(offset)` to resume a download, or `ByteRange::Range { offset, len }` for a bounded slice.

### Partial reads (byte ranges)

Useful for previews, thumbnails, streaming media, or random access into large files:

```rust
// First 1 MB of a file
let head = storage.read_range(file.handle, 0, 1024 * 1024).await?;

// Read from the middle
let middle = storage.read_range(file.handle, 5_000_000, 100_000).await?;

// Offsets past 4 GB work on devices that advertise the 64-bit partial read
// (most modern Android devices do): 64 KB at offset 6 GB.
let chunk = storage.read_range(file.handle, 6 * 1024 * 1024 * 1024, 65536).await?;
```

`read_range` reads at most `u32::MAX` bytes per call. For a bigger span, stream it with
`download(handle, ByteRange::Range { offset, len })` or walk it with `download_windowed`.

### Listen for events

`next_event()` awaits indefinitely, so wrap it in a timeout to allow checking for shutdown, etc.:

```rust
use tokio::time::{timeout, Duration};

loop {
    match timeout(Duration::from_millis(200), device.next_event()).await {
        Ok(Ok(event)) => match event {
            DeviceEvent::ObjectAdded { handle } => {
                println!("New file: {:?}", handle);
            }
            DeviceEvent::StoreRemoved { storage_id } => {
                println!("Storage unplugged: {:?}", storage_id);
            }
            _ => {}
        },
        Ok(Err(Error::Disconnected)) => break,
        Ok(Err(e)) => eprintln!("Error: {}", e),
        Err(_) => continue, // Timeout, check for shutdown, etc.
    }
}
```

## Virtual device (testing without hardware)

The `virtual-device` feature lets you test MTP client code against a local filesystem directory instead of a real USB
device. Enable it in your `Cargo.toml`:

```toml
[dev-dependencies]
mtp-rs = { version = "0.17", features = ["virtual-device"] }
```

### Direct usage

```rust
use std::path::PathBuf;
use mtp_rs::{MtpDevice, VirtualDeviceConfig, VirtualStorageConfig};

let device = MtpDevice::builder()
    .open_virtual(VirtualDeviceConfig {
        manufacturer: "Google".into(),
        model: "Virtual Pixel 9".into(),
        serial: "virtual-001".into(),
        storages: vec![VirtualStorageConfig {
            description: "Internal Storage".into(),
            capacity: 64 * 1024 * 1024 * 1024,
            backing_dir: PathBuf::from("/tmp/mtp-test"),
            read_only: false,
        }],
        watch_backing_dirs: true,
        ..Default::default()
    })
    .await?;
```

`VirtualDeviceConfig` implements `Default`, so set only the fields you care about and spread the rest. `storages` is
the one you always have to fill in: only you know which directory backs the storage, and opening a device with none
fails with "VirtualDeviceConfig requires at least one storage". Building configs this way also means a new field in a
future release won't break your test setup.

When `watch_backing_dirs` is `true`, the virtual device watches its backing directories for external changes (files
created or removed outside of MTP) and emits `ObjectAdded`/`ObjectRemoved` events, just like a real device would.
Set it to `false` in tests that don't need this for faster startup.

### Discovery registry

You can also register virtual devices so they appear in `MtpDevice::list_devices()` and can be opened with
`open_by_location()` or `open_by_serial()`:

```rust
use mtp_rs::{register_virtual_device, unregister_virtual_device};

let info = register_virtual_device(&config);

// Now discoverable
let device = MtpDevice::builder().open_by_serial("virtual-001").await?;

// Clean up when done
unregister_virtual_device(info.location_id);
```

## API overview

The library has two layers:

### High-level API (`mtp::`)

This is what most people want. Friendly types, automatic session management, streaming.

- `MtpDevice` - Connect to devices, get info, list storages
- `Storage` - File operations (list, download, upload, delete, move, copy)
- `DownloadStream` - Streaming downloads with progress
- `DeviceEvent` - Events from the device

### Low-level API (`ptp::`)

For when you need raw protocol access (for cameras or maybe debugging).

- `PtpDevice` - Raw device connection
- `PtpSession` - Manual session control, raw operations
- `OperationCode`, `ResponseCode` - Protocol constants
- Container types for building/parsing protocol messages

With this, you can copy stuff to/from cameras, but there are no other features like reading the battery level,
trigger capture, read supported formats/sizes, etc. This is intentional, didn't want to bloat the library with
camera-specific code because this is mainly for MTP and file transfer.

## Runtime compatibility

The library uses `futures` traits and is runtime-agnostic. It's tested with tokio but should work with async-std or any
other runtime.

We use `nusb` for USB access, which is also runtime-agnostic.

## Known limitations

| Limitation                | Details                                            |
|---------------------------|----------------------------------------------------|
| Files >4GB (size field)   | The wire `ObjectInfo::size` is u32 and caps at 4 GB; the library resolves the real size from the `ObjectSize` object property where the device supports it. Byte-range reads beyond 4 GB use the 64-bit partial read (tested end-to-end on Pixel 9 Pro XL with an 8 GB file). |
| Filename length           | Max 254 characters                                 |
| Non-empty folder delete   | Fails; delete contents first                       |
| One connection per device | Can't open the same device twice                   |
| Upload cancellation       | A partial object may remain on the device. `upload()` surfaces its handle via `UploadError::partial` so you can delete it or resume; the library never auto-deletes it. |
| Recursive listing speed   | Manual traversal is slower (~1 request per folder) |

## Android weirdnesses

Android's MTP implementation has some quirks that this library handles automatically:

- **Behavior:** Recursive listing broken
    - **What happens:** `ObjectHandle::ALL` returns incomplete results (folders only, no files)
    - **How we handle it:** Auto-detected; uses manual folder traversal instead. Although, note that it takes a lot more
      time! Like, if the device supported this, it'd be pretty fast, while with the workaround, in the tests it took
      9 minutes to list ~20k files in ~2k folders.
- **Behavior:** Strict about how you name the storage root when creating
    - **What happens:** Android maps only `0xFFFFFFFF` to the storage root when you create a file or folder there, and
      answers anything else with `InvalidObjectHandle`
    - **How we handle it:** We send the value the MTP spec asks for, so `parent = None` creates in the root
- **Behavior:** Large responses span transfers
    - **What happens:** Data >64KB comes in multiple USB transfers
    - **How we handle it:** Automatically reassembled before parsing
- **Behavior:** Composite USB devices
    - **What happens:** Most phones report as USB class 0 (composite)
    - **How we handle it:** We inspect interfaces to find MTP

The library detects Android devices via the `"android.com"` vendor extension and applies appropriate handling
automatically.
You generally don't need to worry about these details.

**Tip**: An Android root holds only directories by convention (`DCIM`, `Download`, `Pictures`, …), so uploading into one
of them keeps your files where the phone's own apps will look for them:

```rust
// Find the Download folder
let objects = storage.list_objects(None).await?;
let download = objects.iter().find(|o| o.filename == "Download").unwrap();

// Upload into Download
storage.upload(Some(download.handle), file_info, data).await?;
```

## Tested devices

"Full support" really means "Full support, except for general Android quirks listed above".

| Device                                                                                                             | Android | Notes                                                                                                         |
|--------------------------------------------------------------------------------------------------------------------|---------|---------------------------------------------------------------------------------------------------------------|
| Google Pixel 9 Pro XL                                                                                              | 15      | Full support                                                                                                  |
| Samsung Galaxy S23 Ultra (SM-S918B)                                                                                | 14      | No root listing                                                                                               |
| [Amazon Kindle Paperwhite 12th Generation (2024)](https://github.com/vdavid/mtp-rs/pull/2#issuecomment-4264713119) | -       | Full support                                                                                                  |
| [Fairphone 5](https://github.com/vdavid/mtp-rs/issues/6#issuecomment-4234861708) (e/OS 3.0.4, LineageOS-derived)   | 13      | Full support                                                                                                  |
| [Garmin Forerunner 955](https://github.com/vdavid/mtp-rs/pull/10)                                                  | -       | Works for app use; integration suite has one failing test under investigation                                 |
| Garmin Venu 2/2S                                                                                                   | -       | Detected via `MTP` interface string (no standard MTP class)                                                   |
| [Panasonic Lumix DMC-TZ61](https://github.com/vdavid/mtp-rs/issues/12)                                             | -       | Read-only PTP camera; reads, streaming download, cancel, and reset all work, writes unsupported by the device |

**Samsung quirk**: Samsung devices return `InvalidObjectHandle` when listing the root folder with handle 0.
The library automatically detects this and falls back to recursive listing with filtering. This is transparent to users.

We welcome reports of other tested devices! Please open an issue or PR with your device model, Android version,
and any issues encountered.

## Benchmarks

mtp-rs is faster than libmtp across every operation we tested, and the gap widens with file size. On a Google Pixel 9
Pro XL (USB, 5 warmup + 10 measured runs per scenario):

| Operation  | Size   | mtp-rs  | libmtp  | Speedup   |
|------------|--------|---------|---------|-----------|
| download   | 1 MB   | 33.9ms  | 45.3ms  | **1.34x** |
| download   | 10 MB  | 258.3ms | 391.1ms | **1.51x** |
| download   | 100 MB | 2.447s  | 9.897s  | **4.04x** |
| upload     | 1 MB   | 76.1ms  | 115.0ms | **1.51x** |
| upload     | 10 MB  | 326.9ms | 345.1ms | **1.06x** |
| upload     | 100 MB | 2.388s  | 2.796s  | **1.17x** |
| list_files | -      | 15.5ms  | 24.9ms  | **1.61x** |

Beyond raw speed, mtp-rs is far more predictable. At 100 MB downloads, libmtp's individual runs ranged from 3.7s to
18.2s (std dev 4.6s, that's 47% of its median). mtp-rs stayed within a 15ms band (std dev 4.7ms, 0.2% of its median).
In practice this means a 100 MB transfer with mtp-rs reliably takes ~2.4s, while with libmtp it could take anywhere from
4s to 18s.

For large-file throughput, a separate test uploaded an 8.06 GB file to a Pixel 9 Pro XL over USB 3.2 in 98.5s,
sustained **83.8 MB/s**. Partial reads at offsets 2 GB, 4.1 GB, and near-EOF all returned correct bytes, confirming
`GetPartialObject64` works end-to-end on Android.

The benchmark tool is included in the repo. [Run it yourself](../../benchmarks/mtp-rs-vs-libmtp/) with
`cargo run -p mtp-bench -- --warmup 5 --runs 10`.

## Comparison with other libraries

### vs libmtp / libmtp-rs

[libmtp](https://github.com/libmtp/libmtp/) is 20+ years old, battle-tested, and very comprehensive.
[libmtp-rs](https://github.com/quebin31/libmtp-rs) provides a Rust interface to it. But:

- `libmtp` is a C library with all the FFI pain that entails
- It has a massive device quirks database for hardware from 2006
- The API is synchronous and callback-heavy
- It pulls in `libusb`, `libudev`, and other system dependencies

In contrast, `mtp-rs` targets modern Android devices that all behave the same way. If you need to support a weird
MP3 player from 2008, use libmtp. If you're building a modern Android sync tool, mtp-rs is a better fit.

### vs existing Rust PTP crates

[ptp](https://crates.io/crates/ptp) and [libptp](https://crates.io/crates/libptp) both use
[libusb](https://github.com/libusb/libusb) v0.3 for USB access, which is a C dependency.

`mtp-rs` uses [nusb](https://crates.io/crates/nusb) instead, which is pure Rust.

Note that `libptp` is much more mature, though!

### vs winmtp

[winmtp](https://crates.io/crates/winmtp) wraps the Windows WPD COM API and is Windows-only. `mtp-rs`
runs on Linux, macOS, and Windows behind one cross-platform API — raw USB on Linux/macOS, and the same
WPD COM stack winmtp uses on Windows.

## Implementation notes

- I used Opus 4.5 extensively for this implementation. I know it's controversial these days, but the bottom line to me
  is that the implementation WORKS, it has a bunch of integration tests which pass, and hey, I can use it to copy data
  to/from my phone and other phones and I can display async progress and I don't need to rely on C libraries. So no
  hate,
  please. If you dislike or distrust AI-gen code, use the alternatives listed above (if you can live with the libmtp
  dependency), handcraft your own Rust implementation, or fork this repo and add your human thing and use it.
  PRs are also welcome.
- For the protocol spec, I tried to use
  usb.org's [Media Transfer Protocol v.1.1 Spec](https://www.usb.org/document-library/media-transfer-protocol-v11-spec-and-mtp-v11-adopters-agreement),
  but it was a pain to get AI agents to work from it, so I've converted it to Markdown. You can find it
  here: https://github.com/vdavid/mtp-v1_1-spec-md

## Contributing

See [CONTRIBUTING.md](../../CONTRIBUTING.md) for guidelines.

## License

MIT OR Apache-2.0, at your option.
