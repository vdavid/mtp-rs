# mtp-bench: mtp-rs vs libmtp benchmark

Compares [mtp-rs](https://crates.io/crates/mtp-rs) (pure Rust) against [libmtp-rs](https://crates.io/crates/libmtp-rs) (C library bindings) for MTP file transfers on a real device.

## Prerequisites

- Rust toolchain (stable)
- `libmtp` system library installed (needed by libmtp-rs)
  - **Linux:** `sudo apt install libmtp-dev` (Debian/Ubuntu) or equivalent
  - **macOS:** `brew install libmtp`
- A real MTP device (Android phone, etc.) connected via USB
- On macOS, you'll need to kill `ptpcamerad` first (see main README)

## How to run

From the repo root:

```bash
cargo run -p mtp-bench
```

This runs all operations (download, upload, list) at all default sizes (1 MB, 10 MB, 100 MB) with both backends, using 1 warmup + 5 measured runs per scenario.

### CLI options

| Flag | Default | Description |
|------|---------|-------------|
| `--runs` | `5` | Number of measured runs per scenario |
| `--warmup` | `1` | Number of warmup runs (excluded from results) |
| `--sizes` | `1MB,10MB,100MB` | Comma-separated file sizes (supports KB, MB, GB) |
| `--operations` | `download,upload,list` | Comma-separated operations to benchmark. mtp-rs only: `stream` (`DownloadPacing::HeldOpen`), `paced` (the default adaptive pacing), and `windowed` (`DownloadPacing::FixedWindow`, one run per `--windows` size); all discard the bytes |
| `--windows` | `8` | Comma-separated window sizes in MiB for `windowed` |
| `--backends` | `mtp-rs,libmtp` | Comma-separated backends to test |
| `--output` | `table` | Output format: `table`, `markdown`, `csv`, or `json` |

### Example commands

Run only downloads at 10 MB with 10 measured runs:

```bash
cargo run -p mtp-bench -- --operations download --sizes 10MB --runs 10
```

Quick smoke test (1 run, no warmup, small file):

```bash
cargo run -p mtp-bench -- --runs 1 --warmup 0 --sizes 1MB
```

Full benchmark matching the README results (10 runs, 5 warmup), output as markdown:

```bash
cargo run -p mtp-bench -- --runs 10 --warmup 5 --output markdown
```

Only test mtp-rs (skip libmtp):

```bash
cargo run -p mtp-bench -- --backends mtp-rs
```

Export results as JSON for further analysis:

```bash
cargo run -p mtp-bench -- --output json > results.json
```

## How it works

1. Connects to the first available MTP device
2. For each (operation, file size, backend) combination:
   - **Download:** uploads a test file to the device (untimed), then times the download, then deletes it
   - **Upload:** times the upload of a test file, then deletes it
   - **List files:** times listing all objects in the storage root
3. Runs warmup iterations first (discarded), then measured iterations
4. Waits 2 seconds between scenarios to let the USB device settle
5. Reports median, mean, std dev, and throughput for each scenario

Each backend connects and disconnects independently per scenario because both open the same USB device and can't coexist.

## Methodology notes

- **Warmup runs** are discarded to account for device-side caching and USB initialization overhead.
- **USB settle delay** (2s) between scenarios prevents connection failures when switching backends.
- **List files caching:** the first list call on a device is often slower due to device-side indexing. Warmup runs help stabilize this.
- **Median** is used as the primary metric because it's more robust to outliers than mean (and libmtp can have significant outliers at large file sizes).
- Test files are filled with a constant byte (`0xAB`). Content doesn't affect MTP transfer speed, but it does mean the data isn't compressible (not that MTP compresses anything).
