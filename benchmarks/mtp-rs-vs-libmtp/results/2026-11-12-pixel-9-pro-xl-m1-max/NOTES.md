# Pixel 9 Pro XL on an M1 Max, 2026-11-12 (#65)

The run `crates/mtp-rs/README.md` § Benchmarks quotes. `REPORT.md` is `--render` of `r1.json`–`r8.json`.

- Device: Google Pixel 9 Pro XL, Android 17 (`CP3A.261005.005`), USB serial `46061FDAS000A4`, file-transfer mode, USB 3.2 Gen 1 (5 Gbit/s).
- Host: Apple M1 Max (MacBookPro18,2), 10 cores, macOS 27.0.1, libmtp 1.1.23 (Homebrew), mtp-rs 0.38.1 at `8df56d43811b`, release build. The box also runs CI runner slots: none had a job during any chunk, and each chunk started at a 1-minute load under 6 (2.3–5.1). Someone was on the box over Remote Desktop throughout.
- All eight chunks in one pass, 10:18–10:57, one code revision, no reruns. Commands: the "Long runs in chunks" block in `../../README.md`, with `--serial 46061FDAS000A4`.
- mtp-rs `upload` is `Storage::upload` with the default pacing (`KeepDates`). The bench sends no date, so on this phone every mtp-rs upload went in windows. `download` is `download_to_vec` (one `GetObject`).
- Host matters: the same phone on the M3 Max laptop (`../2026-11-11-pixel-9-pro-xl-m3-max/`, an older rev, incomplete) downloaded 1 GB in 4.10 s (~262 MB/s) against 5.97 s (~168 MB/s) here, and libmtp moved the same way. Don't compare numbers across hosts.
- mtp-rs's 1 MB upload is slower than libmtp's here (82.9 ms against 71.6 ms). On the M3 run, when uploads were still held open, it was faster (59.6 against 65.2 ms). Likely the windowed shape's extra requests (free-space check, `SendObject` with no data, `BeginEditObject`, `EndEditObject`) on a file that fits in one window anyway. Not measured in isolation.
- The phone was left as found: no test objects left in `/` or `/Download`.
