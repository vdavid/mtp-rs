# mtp-rs-cli

[![Crates.io](https://img.shields.io/crates/v/mtp-rs-cli)](https://crates.io/crates/mtp-rs-cli)
[![CI](https://github.com/vdavid/mtp-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/vdavid/mtp-rs/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/mtp-rs-cli)](LICENSE-MIT)

A universal MTP file transfer CLI built on [`mtp-rs`](https://crates.io/crates/mtp-rs).

Talk to Android phones, Kindles, Garmin watches, media players, and other MTP devices with the same handful of commands. No `libmtp`, no `libusb`, no FFI. Pure Rust.

## Install

```sh
cargo install mtp-rs-cli
```

The installed binary is called `mtp-rs`.

## Quick tour

```sh
# Show visible devices.
mtp-rs devices
mtp-rs devices --json

# Inspect a specific device and its storages.
mtp-rs info --device SERIAL

# List files.
mtp-rs ls /
mtp-rs ls /Music --recursive

# Upload and download files.
mtp-rs put ./song.mp3 /Music/song.mp3 --replace
mtp-rs get /Music/song.mp3 ./song.mp3
mtp-rs get /DCIM/Camera ./Camera

# Create and remove remote objects.
mtp-rs mkdir /Upload
mtp-rs rm /Upload/old.bin --yes

# Rename, move, and copy remote objects.
mtp-rs rename /Upload/old.bin new.bin
mtp-rs mv /Upload/new.bin /Archive/new.bin
mtp-rs cp /Archive/new.bin /Archive/new-copy.bin

# Diagnose access, storage, and common USB ownership issues.
mtp-rs doctor

# Recover a stuck device after an interrupted transfer (no replug needed).
mtp-rs reset
```

Remote paths are POSIX-like and absolute: `/`, `/Music/song.mp3`, `/GARMIN/APPS/app.prg`. Device-specific workflows are just normal file operations. For example, Garmin Connect IQ sideloading is:

```sh
mtp-rs put ./MyApp.prg /GARMIN/APPS/MyApp.prg --replace --verify
```

## What you get

- **One CLI, every device**: phones, watches, e-readers, media players. If it speaks MTP, this works.
- **JSON output** with `--json`: every command emits structured JSON for shell pipelines and scripts.
- **Stable exit codes**: 0 success, 2 no device, 3 ambiguous selection, 4 access denied, 5 bad remote path, 6 transfer error, 7 verification failed.
- **Streaming transfers**: large uploads and downloads stream straight from disk to USB and back. No silent in-memory buffering.
- **`--verify` for uploads**: read the file back from the device and byte-compare it against the local source.
- **Cross-platform**: Linux, macOS, Windows.

## Docs

See [docs/cli.md](docs/cli.md) for the full command reference: every flag, JSON schemas, exit codes, path semantics, and troubleshooting.

## Platform notes

The CLI inherits the platform quirks of the underlying [`mtp-rs`](https://crates.io/crates/mtp-rs) library. The big one to know about is macOS:

- macOS ships `ptpcamerad` which grabs MTP devices on connect. You may need to kill it (`pkill -9 ptpcamerad`) or disable it (`sudo launchctl disable system/com.apple.ptpcamerad`) before the CLI can open a device.
- Quit `Android File Transfer.app` if you have it installed. It also grabs devices on connect.
- Recent macOS can deny USB access to processes started from background agents or random tool runners. If `mtp-rs devices` lists a device but `info` or `get` fails, try running from Terminal or iTerm with the Mac unlocked.
- On Linux, you may need udev rules. See the [lib README](../mtp-rs/README.md) for the snippet.
- Windows should work out of the box but is not extensively tested.

## Garmin sideloading example

```sh
# Find the watch.
mtp-rs devices

# Push a Connect IQ app to the watch, replace if it exists, verify after.
mtp-rs put ./MyField.prg /GARMIN/APPS/MyField.prg --replace --verify
```

The CLI auto-detects Garmin watches that expose MTP as a vendor-class interface with an `MTP` interface string (Venu 2/2S, and likely others).

## Contributing

See [CONTRIBUTING.md](../../CONTRIBUTING.md) in the repo root.

## Credits

Originally contributed by [Dmitry Tretyakov](https://github.com/dtretyakov) in [#11](https://github.com/vdavid/mtp-rs/pull/11). Maintained by [David Veszelovszki](https://github.com/vdavid).

## License

MIT OR Apache-2.0, at your option.
