# Contributing to mtp-rs

Thanks for considering contributing! This document covers the practical stuff you need to know.

## Getting started

```bash
git clone https://github.com/vdavid/mtp-rs
cd mtp-rs
cargo build
cargo test
```

You don't need an MTP device for most development. The test suite uses mock transports for protocol logic.

### Dev tools

We use [`just`](https://github.com/casey/just) as a command runner. Install it, then:

```bash
just            # Run all checks: format, lint, test, doc
just fix        # Auto-fix formatting and clippy warnings
just check-all  # Include MSRV check, security audit, and license check
```

Run `just --list` to see all available commands.

### MSRV (Minimum Supported Rust Version)

We support the Rust releases from the last 12 months, currently Rust 1.91. Raising it is a minor version bump with a changelog line. Before submitting PRs, verify MSRV compatibility:

```bash
rustup toolchain install 1.91.0  # One-time setup
just msrv                         # Check MSRV compatibility
```

This catches issues that only appear on older Rust versions. CI runs this check, so `just msrv` locally saves a round-trip.

The MSRV covers what consumers compile: the library (every feature) and the CLI binary. Tests and dev-dependencies only need stable Rust, so a dev-dependency may require a newer toolchain. `just msrv` resolves dependencies afresh the way a consumer on the MSRV would, so `Cargo.lock` may hold versions that need a newer Rust, and `Cargo.toml` never caps a dependency for the MSRV. Details in [docs/dependency-updates.md](docs/dependency-updates.md).

## Project structure

The repo is a Cargo workspace with two published crates and one internal benchmark crate.

```
crates/
├── mtp-rs/                       # The library (published as `mtp-rs`)
│   ├── src/
│   │   ├── ptp/                  # Low-level protocol implementation
│   │   │   ├── codes.rs          # Operation/response/event code enums
│   │   │   ├── pack/             # Binary serialization (little-endian, UTF-16LE strings)
│   │   │   ├── container.rs      # USB container format
│   │   │   ├── session/          # Session management and operations
│   │   │   └── device.rs         # PtpDevice public API
│   │   ├── mtp/                  # High-level API
│   │   │   ├── device.rs         # MtpDevice and builder
│   │   │   ├── storage.rs        # Storage and file operations
│   │   │   ├── stream.rs         # Streaming downloads
│   │   │   ├── object.rs         # NewObjectInfo for uploads
│   │   │   └── event.rs          # Device events
│   │   ├── transport/            # USB abstraction
│   │   │   ├── mock.rs           # Mock for testing
│   │   │   ├── nusb.rs           # Real USB implementation
│   │   │   └── virtual_device/   # Filesystem-backed virtual device
│   │   ├── error.rs              # Error types
│   │   └── lib.rs                # Crate root
│   ├── tests/integration.rs      # Tests that need a real device
│   └── examples/                 # Standalone examples (download, diagnose, etc.)
└── mtp-rs-cli/                   # The CLI binary (published as `mtp-rs-cli`)
    ├── src/
    │   ├── main.rs               # Entry point
    │   └── cli/                  # Command dispatch, args, error mapping, paths
    ├── tests/cli.rs              # Cross-process tests against the built binary
    └── docs/cli.md               # Full command reference

benchmarks/
└── mtp-rs-vs-libmtp/             # Throughput/predictability comparison vs libmtp

docs/                             # Protocol, architecture, debugging, release process
```

## Running tests

```bash
# Unit tests (no device needed): runs the whole workspace
cargo test --workspace

# Library integration tests (real MTP device connected)
cargo test -p mtp-rs --test integration -- --ignored --nocapture --test-threads=1

# CLI tests (use a virtual device, no real hardware needed)
cargo test -p mtp-rs-cli --features virtual-device
```

The integration tests are split into read-only (safe) and destructive (creates/deletes files) to avoid messing up
your device if you don't trust the lib too much but still want to run some tests.

Integration tests run serially to avoid the obvious collisions.

### Picking a writable folder for destructive tests

Destructive tests need a folder they can write into. By default they walk a priority list of common folder names and
use the first one that exists in the storage root:

```
Download → Downloads → Music → Documents → documents → Pictures → Audiobooks → Podcasts
```

This covers Android (`Download`), Garmin (`Music`), Kindle (`documents`), and most other modern devices. If your
device exposes a differently-named writable folder, override with `MTP_TEST_FOLDER`:

```bash
MTP_TEST_FOLDER=Internal cargo test --test integration destructive -- --ignored --nocapture --test-threads=1
```

When no match is found, destructive tests skip with a clear log line instead of panicking. If you're testing a new
device, please [share results in #6](https://github.com/vdavid/mtp-rs/issues/6) so we can keep the priority list and
README's tested-devices table up to date.

## Code style

We follow standard Rust conventions. See [`docs/style-guide.md`](docs/style-guide.md) for details.

The quick version:

- Run `just` before committing (or `just fix` to auto-fix issues)
- Tests for new functionality
- Doc comments for public APIs

No need to over-document internal code. If the code is clear, a brief comment or none at all is fine.

## Architecture decisions

A few things that might not be obvious:

- **Two-layer API**: The `ptp::` module is the protocol implementation, `mtp::` is the user-friendly wrapper. Most
  changes to user-facing behavior go in `mtp::`, protocol fixes go in `ptp::`.
- **Runtime agnostic**: We don't depend on tokio directly. Use `futures` traits and `futures-timer` for timeouts. This
  lets users bring their own runtime.
- **No device quirks**: Unlike libmtp, we don't have a quirks database. Modern Android devices all behave the same way.
  If you find a device that doesn't work, let's understand why before adding workarounds.
- **Mock transport for testing**: `transport::mock::MockTransport` lets you test protocol logic without USB. Queue
  expected responses and verify sent commands.
- **`VirtualDeviceConfig` stays literal-constructible**: every field is public and consumers build it with a struct
  literal, so give any new field a value in the `Default` impl and don't reach for `#[non_exhaustive]`. That keeps
  `VirtualDeviceConfig { storages: vec![...], ..Default::default() }` compiling across releases, which is the whole
  point: a field addition to a test-support struct shouldn't break downstream test suites. Add the field to the
  `default_config_is_complete_except_storages` test too. `VirtualStorageConfig` has no `Default` on purpose, because
  an unset `backing_dir` yields a silently empty storage instead of an error.

## What we're looking for

- Testing with real devices and updating [README.md](README.md#tested-devices) with more device info
- Bug reports (with reproduction steps)
- Docs improvements
- Maybe some more PTP implementations like battery level, I'm unsure if we really want it or we should focus on MTP

We're not really looking to add legacy stuff like:

- MTPZ support
- Legacy device quirks
- Playlist/metadata syncing
- Vendor extensions

These don't seem that useful.

## The protocol

If you need to understand MTP/PTP, see the docs:

- [`docs/protocol.md`](docs/protocol.md) - Wire format, operations, data structures
- [`docs/architecture.md`](docs/architecture.md) - Module structure and design decisions
- [`docs/debugging.md`](docs/debugging.md) - USB capture for troubleshooting
- [`mtp-v1_1-spec-md`](https://github.com/vdavid/mtp-v1_1-spec-md) - Separate repo. The full MTP spec. Reference only, it's dense.

The protocol is essentially:

1. Send a command container (operation code + params)
2. Optionally send/receive data containers
3. Receive a response container (success/error code + params)

Everything is little-endian. Strings are UTF-16LE with a length prefix.

## Releasing

See [`docs/releasing.md`](docs/releasing.md) for how to publish a new version to crates.io.

## Submitting changes

1. Fork and create a branch
2. Make your changes
3. Run `just` (checks format, lint, test, and doc)
4. Run `just msrv` to verify Rust 1.91 compatibility
5. If you have a device, run integration tests: `cargo test --test integration -- --ignored`
6. Open a PR with a clear description including how you tested your changes

For non-trivial changes, consider opening an issue first to discuss the approach.

## Questions?

Open an issue. We're happy to chat!
