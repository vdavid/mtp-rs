# Pixel 9 Pro XL on an M3 Max laptop, 2026-11-11 (#65): incomplete, superseded

- Device: Google Pixel 9 Pro XL, Android 17 (`CP3A.261005.005`), USB 3.2 Gen 1 (5 Gbit/s). Host: Apple M3 Max (Mac15,9), macOS 27.0.1, libmtp 1.1.23, mtp-rs 0.38.1. These reports predate the bench's `host` field, so it's empty in them.
- Code rev of `r1`–`r7`: `255621b1c982` (the bench commits on `691e1f6`). Release builds. 1-minute load 2.7–5.4 at each chunk's start.
- `r8` (`list_during_windowed`) never finished: two tries hit load spikes from other builds on the laptop (load 30–110). The spoiled one is `earlier-rev/r8-load-spike.json`.
- `earlier-rev/`: a first pass at rev `5a007e8c5f2e`, kept for reference.
- The laptop was too loaded to finish, so the full run moved to a quieter host at a later revision: `../2026-11-12-pixel-9-pro-xl-m1-max/`. That's the one the README quotes. Don't merge these reports with it: different host and code.
