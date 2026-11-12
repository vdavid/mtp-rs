# mtp-bench: mtp-rs vs libmtp benchmark

Compares [mtp-rs](https://crates.io/crates/mtp-rs) (pure Rust) against [libmtp-rs](https://crates.io/crates/libmtp-rs) (C library bindings) for MTP file transfers on a real device, and measures what mtp-rs's paced downloads cost and buy.

## Prerequisites

- Rust toolchain (stable)
- For the libmtp baseline, the `libmtp` system library (libmtp-rs links the system copy through `pkg-config`)
  - **Linux:** `sudo apt install libmtp-dev` (Debian/Ubuntu) or equivalent
  - **macOS:** `brew install libmtp`
  - **Without it:** `--no-default-features` turns off the `libmtp` feature and builds an mtp-rs-only bench
  - **Windows:** never built there (no libmtp that `pkg-config` finds, and Windows reaches phones through WPD), so no flag is needed; see "On Windows" below
- A real MTP device (Android phone, etc.) connected via USB, with a `Download` folder in its first storage and enough free space for the biggest size (4 GB with `list_during`)
- On macOS, you'll need to stop `ptpcamerad` first (see `docs/debugging.md`)

A build without libmtp defaults to `--backends mtp-rs`, refuses `--backends libmtp` with the reason, and says in the report header which backends it had.

## How to run

From the repo root, always with `--release`: a debug build of mtp-rs can't keep up with USB 3, while libmtp is optimized C either way.

```bash
cargo run --release -p mtp-bench
```

This runs all operations (download, upload, list) at all default sizes (1 MB, 10 MB, 100 MB, 1 GB) with both backends, using 1 warmup + 5 measured runs per scenario.

### CLI options

| Flag | Default | Description |
|------|---------|-------------|
| `--runs` | `5` | Number of measured runs per scenario |
| `--warmup` | `1` | Number of warmup runs (excluded from results) |
| `--sizes` | `1MB,10MB,100MB,1GB` | Comma-separated file sizes (supports KB, MB, GB, decimal) |
| `--list-during-sizes` | `4GB` | File sizes for `list_during` |
| `--operations` | `download,upload,list` | Comma-separated operations to benchmark. mtp-rs only: `stream` (`DownloadPacing::HeldOpen`), `paced` (the default adaptive pacing), `windowed` (`DownloadPacing::FixedWindow`, one run per `--windows` size), and `list_during` (see below; `list_during_held`, `list_during_paced`, and `list_during_windowed` pick one pacing); the download modes discard the bytes |
| `--windows` | `8` | Comma-separated window sizes in MiB for `windowed` and `list_during` |
| `--backends` | every backend the build has | Comma-separated backends to test: `mtp-rs,libmtp`, or `mtp-rs` without libmtp |
| `--output` | `table` | Output format: `table`, `markdown`, `csv`, or `json` |
| `--save` | none | Also write the JSON report to this file. Use it rather than `--output json > file`: libmtp prints device detection to stdout, which lands after the JSON |
| `--render` | none | Comma-separated JSON reports to merge and print in `--output` format, with no device (see "Long runs in chunks") |
| `--serial` | none | The device to benchmark, by USB serial (`mtp-rs devices` lists them). Required when more than one MTP device is connected: neither backend guesses |

### Picking the device

With `--serial`, mtp-rs opens that device by serial. On Windows it opens the phone through WPD, whose serial is the MTP one (a Pixel's is 32 hex digits), so there the bench looks the USB serial up in the listing and opens that USB location, which mtp-rs maps to the WPD device; a serial the listing doesn't know is tried as a WPD serial. libmtp's raw device list has no serial without opening each device, and opening another phone could disturb whoever is using it, so the bench turns the serial into a USB vendor and product ID through mtp-rs's listing (which opens nothing) and opens the one raw device with that ID. Two connected devices of the same model can't be told apart this way, and the bench refuses rather than guess; after opening, it checks the device's serial too. Without `--serial`, both backends need exactly one MTP device connected.

### Why these sizes

- **1 GB in `--sizes`**: paced downloads read in windows of about two seconds (the first one is 8 MiB). At the ~168–268 MB/s a Pixel 9 Pro XL reaches on USB 3 (the host decides where: an M1 Max at the low end, an M3 Max at the high one), a 100 MB file is the first window plus one more, so it can't show what pacing costs. 1 GB takes about three windows.
- **4 GB for `list_during`**: the listing is issued halfway, so held open it waits for the rest of the file. At 268 MB/s that's ~7.5 s for 4 GB against at most one ~2 s window paced; at 1 GB the two would look alike (~1.9 s left, one window's worth). 4 GB also stays under 4 GiB, so the run doesn't cross into the over-4-GiB size path as well. On USB 2 (~40 MB/s) each 4 GB download takes ~100 s, so expect a long run there.

### Listing during a download (`list_during`)

The point of paced downloads is that the device stays usable while a big file comes off it. `list_during` measures that directly: it downloads a `--list-during-sizes` file and, once half of it has arrived, lists the storage root on the same session, the way a file manager refreshes a folder while a copy runs. Each run records two numbers:

- **Download**: start to last byte, including the listing's turn. This is the cost of pacing.
- **Listing**: from issuing the listing to its answer. This is the benefit.

`list_during` expands to three pacings, each its own scenario: held open (the listing waits for the whole rest of the file), the default (~2 s windows), and a fixed window per `--windows` size. The file is uploaded once per scenario, untimed, and read by every run. The report prints these in their own table, next to the main one. Add `list` to the operations to see an idle listing's time for comparison.

### What the report records

Every output starts with the run's context, so a table can be trusted on its own:

- Date, host machine (CPU, plus the model identifier on macOS or the maker and model on Windows), and host OS and version.
- mtp-rs version and git commit (`-dirty` with local changes). A binary copied to a machine without the checkout reports the commit it was built from, marked `(at build time)`.
- The system libmtp version (or why the build has none) and the backends the build has.
- The device's name and firmware, and the negotiated USB link speed (the slowest of port, cable, and device, from `MtpDeviceInfo::speed`). A USB 3 phone on a USB 2 cable reports USB 2, which is the first thing to check when numbers look low. On Windows the speed still comes from the USB listing (nusb asks the hub, whatever driver the phone is bound to), so it's known there too.
- How mtp-rs's uploads went out: in windows (Android's edit operations, over USB) or in one piece (WPD, cameras). Upload rows from the two aren't the same operation.
- Host load at start and end: the 1, 5, and 15-minute load averages on macOS and Linux. Windows has no load average, so there it's the CPU's busy share over one second (from `GetSystemTimes`), and at the end also the busy share over the whole run.

`csv` writes the context as `#` comment lines; `json` writes `{ "run": {...}, "results": [...] }`.

### Example commands

Run only downloads at 10 MB with 10 measured runs:

```bash
cargo run --release -p mtp-bench -- --operations download --sizes 10MB --runs 10
```

Quick smoke test (1 run, no warmup, small file):

```bash
cargo run --release -p mtp-bench -- --runs 1 --warmup 0 --sizes 1MB
```

Full benchmark matching the README results (10 runs, 5 warmup), output as markdown:

```bash
cargo run --release -p mtp-bench -- --runs 10 --warmup 5 --output markdown
```

What pacing costs and buys (mtp-rs only):

```bash
cargo run --release -p mtp-bench -- --backends mtp-rs \
    --operations stream,paced,windowed,list_during,list --windows 8,32 --output markdown
```

Only test mtp-rs (skip libmtp):

```bash
cargo run --release -p mtp-bench -- --backends mtp-rs
```

Export results as JSON for further analysis:

```bash
cargo run --release -p mtp-bench -- --save results.json
```

### Long runs in chunks

The full set takes most of an hour on USB 3, longer on USB 2. To run it in pieces (to watch the host's load between them, or to stay under a shell timeout), save each piece with `--save` and merge them with `--render`, which opens no device, refuses pieces from different devices, links, hosts, or versions, and warns about pieces from different commits (the header then lists them all). `list_during_held`, `list_during_paced`, and `list_during_windowed` select one pacing of `list_during` each. Keep each scenario in a single piece: a scenario split across two pieces shows up twice.

```bash
B="./target/release/mtp-bench --serial <serial> --output markdown"
cargo build --release -p mtp-bench
$B --sizes 1MB,10MB,100MB --warmup 5 --runs 10 --save r1.json
$B --sizes 1GB --operations download --backends mtp-rs --warmup 5 --runs 10 --save r2.json
$B --sizes 1GB --operations download --backends libmtp --warmup 5 --runs 10 --save r3.json
$B --sizes 1GB --operations upload --warmup 5 --runs 10 --save r4.json
$B --sizes 1GB --backends mtp-rs --operations stream,paced --warmup 2 --runs 8 --save r5.json
$B --sizes 1GB --backends mtp-rs --operations windowed --windows 8,32 --warmup 2 --runs 8 --save r6.json
$B --backends mtp-rs --operations list_during_held,list_during_paced --warmup 1 --runs 7 --save r7.json
$B --backends mtp-rs --operations list_during_windowed --windows 8,32 --warmup 1 --runs 7 --save r8.json
./target/release/mtp-bench --render r1.json,r2.json,r3.json,r4.json,r5.json,r6.json,r7.json,r8.json --output markdown
```

### On Windows

The bench runs mtp-rs only there, through the WPD backend (`mtp::backend::wpd`), with no flag: libmtp isn't built on Windows. Every operation works:

- `download`, `stream`, `paced`, `windowed`, `list`, and the `list_during` family go through WPD as they do through USB elsewhere. Paced and windowed downloads are real MTP partial reads through the driver's passthrough where it allows them (see `AGENTS.md` § Windows WPD backend).
- `upload` works but goes out **in one piece**: WPD has no windowed uploads, while the Mac run's mtp-rs uploads went in windows. The header's `Uploads:` line says which, so compare upload rows across the two with that in mind.

**Build** with the MSVC toolchain (the GNU one breaks on a non-ASCII profile path). For a machine with no toolchain, link the C runtime statically so the binaries run without the Visual C++ runtime installed, and pass `--target` so the flag stays off build scripts:

```powershell
$env:RUSTFLAGS = "-C target-feature=+crt-static"
cargo build --release --target x86_64-pc-windows-msvc -p mtp-bench -p mtp-rs-cli
cargo build --release --target x86_64-pc-windows-msvc -p mtp-rs --example paced_with_calls
# target\x86_64-pc-windows-msvc\release\mtp-bench.exe, mtp-rs.exe, examples\paced_with_calls.exe
```

The copied binary reports the commit it was built from (`(at build time)` in the header), since there's no checkout to ask.

**Run** from the folder with the binaries, in PowerShell. Check the phone shows up and note its link speed first (`.\mtp-rs.exe devices`); with only that phone connected, `--serial` can be left out. The same chunks as the Mac run, minus libmtp's own:

```powershell
$S = "--serial", "<serial>", "--output", "markdown"
.\mtp-bench.exe @S --sizes 1MB,10MB,100MB --warmup 5 --runs 10 --save r1.json
.\mtp-bench.exe @S --sizes 1GB --operations download --warmup 5 --runs 10 --save r2.json
.\mtp-bench.exe @S --sizes 1GB --operations upload --warmup 5 --runs 10 --save r4.json
.\mtp-bench.exe @S --sizes 1GB --operations stream,paced --warmup 2 --runs 8 --save r5.json
.\mtp-bench.exe @S --sizes 1GB --operations windowed --windows 8,32 --warmup 2 --runs 8 --save r6.json
.\mtp-bench.exe @S --operations list_during_held,list_during_paced --warmup 1 --runs 7 --save r7.json
.\mtp-bench.exe @S --operations list_during_windowed --windows 8,32 --warmup 1 --runs 7 --save r8.json
.\mtp-bench.exe --render r1.json,r2.json,r4.json,r5.json,r6.json,r7.json,r8.json --output markdown
```

Close File Explorer windows on the phone while it runs: Explorer talks to the same WPD device, and its requests queue with the bench's.

## How it works

1. Opens the first MTP device briefly to record its name, firmware, and link speed
2. For each (operation, file size, backend) combination:
   - **Download:** uploads a test file to the device (untimed), then times the download, then deletes it
   - **Upload:** times the upload of a test file, then deletes it
   - **List files:** times listing all objects in the storage root
   - **List during:** uploads one test file (untimed), then each run times a download with a listing issued halfway
3. Runs warmup iterations first (discarded), then measured iterations
4. Waits 2 seconds between scenarios to let the USB device settle
5. Reports median, mean, std dev, and throughput for each scenario

Each backend connects and disconnects independently per scenario because both open the same USB device and can't coexist.

## Methodology notes

- **Warmup runs** are discarded to account for device-side caching and USB initialization overhead.
- **USB settle delay** (2s) between scenarios prevents connection failures when switching backends.
- **List files caching:** the first list call on a device is often slower due to device-side indexing. Warmup runs help stabilize this.
- **Median** is used as the primary metric because it's more robust to outliers than mean (and libmtp can have significant outliers at large file sizes).
- Test files are filled with a constant byte (`0xAB`). Content doesn't affect MTP transfer speed (MTP doesn't compress anything). mtp-rs uploads stream them in 1 MiB chunks, so a 4 GB file needs no 4 GB buffer; libmtp's upload takes a buffer of the whole file.
- **The listing in `list_during` runs in the same task as the download** (`futures::join!`), the way an app with one device handle would issue it. It queues for the device while the download keeps reading, and gets in at the next window boundary.
