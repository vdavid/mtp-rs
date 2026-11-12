# mtp-bench: mtp-rs vs libmtp benchmark

Compares [mtp-rs](https://crates.io/crates/mtp-rs) (pure Rust) against [libmtp-rs](https://crates.io/crates/libmtp-rs) (C library bindings) for MTP file transfers on a real device, and measures what mtp-rs's paced downloads cost and buy.

## Prerequisites

- Rust toolchain (stable)
- `libmtp` system library installed (needed by libmtp-rs, which links the system copy through `pkg-config`)
  - **Linux:** `sudo apt install libmtp-dev` (Debian/Ubuntu) or equivalent
  - **macOS:** `brew install libmtp`
- A real MTP device (Android phone, etc.) connected via USB, with a `Download` folder in its first storage and enough free space for the biggest size (4 GB with `list_during`)
- On macOS, you'll need to stop `ptpcamerad` first (see `docs/debugging.md`)

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
| `--backends` | `mtp-rs,libmtp` | Comma-separated backends to test |
| `--output` | `table` | Output format: `table`, `markdown`, `csv`, or `json` |
| `--save` | none | Also write the JSON report to this file. Use it rather than `--output json > file`: libmtp prints device detection to stdout, which lands after the JSON |
| `--render` | none | Comma-separated JSON reports to merge and print in `--output` format, with no device (see "Long runs in chunks") |
| `--serial` | none | The device to benchmark, by USB serial (`mtp-rs devices` lists them). Required when more than one MTP device is connected: neither backend guesses |

### Picking the device

With `--serial`, mtp-rs opens that device by serial. libmtp's raw device list has no serial without opening each device, and opening another phone could disturb whoever is using it, so the bench turns the serial into a USB vendor and product ID through mtp-rs's listing (which opens nothing) and opens the one raw device with that ID. Two connected devices of the same model can't be told apart this way, and the bench refuses rather than guess; after opening, it checks the device's serial too. Without `--serial`, both backends need exactly one MTP device connected.

### Why these sizes

- **1 GB in `--sizes`**: paced downloads read in windows of about two seconds (the first one is 8 MiB). At the ~168–268 MB/s a Pixel 9 Pro XL reaches on USB 3 (the host decides where: an M1 Max at the low end, an M3 Max at the high one), a 100 MB file is the first window plus one more, so it can't show what pacing costs. 1 GB takes about three windows.
- **4 GB for `list_during`**: the listing is issued halfway, so held open it waits for the rest of the file. At 268 MB/s that's ~7.5 s for 4 GB against at most one ~2 s window paced; at 1 GB the two would look alike (~1.9 s left, one window's worth). 4 GB also stays under 4 GiB, so the run doesn't cross into the over-4-GiB size path as well. On USB 2 (~40 MB/s) each 4 GB download takes ~100 s, so expect a long run there.

### Listing during a download (`list_during`)

The point of paced downloads is that the device stays usable while a big file comes off it. `list_during` measures that directly: it downloads a `--list-during-sizes` file and, once half of it has arrived, lists the storage root on the same session, the way a file manager refreshes a folder while a copy runs. Each run records two numbers:

- **Download**: start to last byte, including the listing's turn. This is the cost of pacing.
- **Listing**: from issuing the listing to its answer. This is the benefit.

`list_during` expands to three pacings, each its own scenario: held open (the listing waits for the whole rest of the file), the default (~2 s windows), and a fixed window per `--windows` size. The file is uploaded once per scenario, untimed, and read by every run. The report prints these in their own table, next to the main one. Add `list` to the operations to see an idle listing's time for comparison.

### What the report records

Every output starts with the run's context, so a table can be trusted on its own: date, host machine (CPU, and the model identifier on macOS), host OS and version, mtp-rs version and git commit (`-dirty` with local changes), the system libmtp version, the device's name and firmware, and the negotiated USB link speed (the slowest of port, cable, and device, from `MtpDeviceInfo::speed`). A USB 3 phone on a USB 2 cable reports USB 2, which is the first thing to check when numbers look low. `csv` writes the context as `#` comment lines; `json` writes `{ "run": {...}, "results": [...] }`.

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
