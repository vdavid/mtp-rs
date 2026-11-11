# Pixel 9 Pro XL run for #65: status

- Device: Google Pixel 9 Pro XL, Android 17 (`CP3A.261005.005`), USB serial `46061FDAS000A4`, USB 3.2 Gen 1 (5 Gbit/s), macOS 27.0.1, libmtp 1.1.23, mtp-rs 0.38.1.
- Code rev of every chunk in this folder: `255621b1c982` (the bench commits cherry-picked onto `691e1f6`, which has windowed uploads and the one-window wait fix). Release builds.
- Done: `r1`–`r7`, 2026-11-11 23:02–23:57, 1-minute load 2.7–5.4 at each chunk's start.
- Remaining: `r8` (`list_during_windowed`, fixed 8 and 32 MiB windows). Two tries hit load spikes from other builds on the host (load 30–110); the spoiled one is `earlier-rev/r8-load-spike.json`, not for the report. It did show listings waiting 41 ms (8 MiB) and 72 ms (32 MiB) median.
- `earlier-rev/`: the first pass at rev `5a007e8c5f2e` (r1, r2, the libmtp 1 GB download with a load spike). Kept for reference; `r2` at the new rev came out 4.10 s against 4.19 s then, the same within noise. The libmtp 1 GB rerun (`r3`) again had slow outliers (10.6 s, 8.3 s against ~6.2 s) at a quiet load, so those are libmtp's own, not only the load spike.
- `paced-download-listings-test.patch`: a hardware test for `tests/integration.rs`, `test_paced_download_with_listings_from_another_task`, the download twin of `test_paced_upload_round_trip_with_listings` (listings from another tokio task, multi-thread runtime, every 250 ms). Not applied yet: it touches `crates/mtp-rs`, which would change the code rev mid-run. Apply it after `r8`, run it on the Pixel, and commit it. Never run.

## Resume

The code rev comes from the last commit that touched code, so `r8` must run on this branch at `255621b` (before a rebase onto newer `main`), or every chunk reruns at the new rev. Phone on USB and unlocked, ptpcamerad blocked (`docs/debugging.md`), `adb kill-server`, load under ~6:

```bash
R=benchmarks/mtp-rs-vs-libmtp/results/2026-11-11-pixel-9-pro-xl
cargo build --release -p mtp-bench
adb shell svc power stayon usb   # then adb kill-server
./target/release/mtp-bench --serial 46061FDAS000A4 --output markdown --backends mtp-rs --operations list_during_windowed --windows 8,32 --warmup 1 --runs 7 --save $R/r8.json
./target/release/mtp-bench --render $R/r1.json,$R/r2.json,$R/r3.json,$R/r4.json,$R/r5.json,$R/r6.json,$R/r7.json,$R/r8.json --output markdown
git apply $R/paced-download-listings-test.patch
MTP_TEST_FOLDER=Download cargo test -p mtp-rs --release --test integration paced_download_with_listings -- --ignored --nocapture
adb start-server; adb shell svc power stayon false
```

Then update `crates/mtp-rs/README.md` § Benchmarks and the headline (line 10).
