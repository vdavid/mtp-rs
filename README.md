# mtp-rs workspace

[![CI](https://github.com/vdavid/mtp-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/vdavid/mtp-rs/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.91-blue)](https://blog.rust-lang.org/2025/10/30/Rust-1.91.0/)

Pure-Rust MTP/PTP for modern Android phones, e-readers, Garmin watches, and cameras. No `libmtp`, no `libusb`, no FFI.
Async, runtime-agnostic, consistently faster than libmtp, and big Android folders list in a fraction of a second.

This repo ships two crates:

| Crate                                  | What it is                                                | Page                                                                             |
|----------------------------------------|-----------------------------------------------------------|----------------------------------------------------------------------------------|
| **[`mtp-rs`](crates/mtp-rs/)**         | The library. Use it from your own Rust code.              | [crates.io](https://crates.io/crates/mtp-rs) · [docs.rs](https://docs.rs/mtp-rs) |
| **[`mtp-rs-cli`](crates/mtp-rs-cli/)** | A ready-made `mtp-rs` binary. `brew install vdavid/tap/mtp-rs`. | [crates.io](https://crates.io/crates/mtp-rs-cli)                                 |

For library usage, the API, the device quirks we handle, and tested devices, see the [
`mtp-rs` README](crates/mtp-rs/README.md).

For the CLI, see the [`mtp-rs-cli` README](crates/mtp-rs-cli/README.md) and
the [full command reference](crates/mtp-rs-cli/docs/cli.md).

## MSRV

Both crates support the Rust releases from the last 12 months, currently Rust 1.91 or newer. Raising this minimum is a
minor version bump, and the changelog always says so.

## Sister projects

- [Cmdr](https://github.com/vdavid/cmdr): an AI-native file manager that uses `mtp-rs` for MTP access.
- [mtp-mount](https://github.com/vdavid/mtp-mount): mount a phone or camera as a normal folder, via FUSE. Unlike gvfs,
  it can also _write_, so `cp` and `rsync` onto the device work.

## Built with mtp-rs

- [Android Bridge](https://android-bridge.com/): a macOS app for moving files to and from an Android phone, with
  drag-and-drop, SHA-256 copy verification, and resumable moves.
- [EyeCanSee](https://github.com/IoSonoPiero/eyecansee): an image and video viewer and manager for Linux and
  Windows that browses, copies, and converts photos and videos on Android phones, disks, and servers.

Built something on `mtp-rs`? Open an issue and I'll add it.

## Development

```sh
just            # fast checks: fmt, clippy, test, doc
just check-all  # plus MSRV, security audit, license check
just fix        # auto-fix formatting and clippy warnings
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for more.

## Credits

The `mtp-rs-cli` crate was originally contributed by [Dmitry Tretyakov](https://github.com/dtretyakov)
in [#11](https://github.com/vdavid/mtp-rs/pull/11). For the full list of contributors to the library, see
the [contributors page](https://github.com/vdavid/mtp-rs/graphs/contributors).

## License

MIT OR Apache-2.0, at your option.
