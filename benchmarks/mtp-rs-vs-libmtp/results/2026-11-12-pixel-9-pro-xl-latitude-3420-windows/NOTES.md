# Pixel 9 Pro XL on a Dell Latitude 3420 under Windows, 2026-11-12 (#67)

The run `crates/mtp-rs/README.md` § Benchmarks quotes for Windows. `REPORT.md` is `--render` of `r1.json`–`r8.json` (no `r3.json`: that's libmtp's 1 GB chunk, and there's no libmtp on Windows).

- Device: Google Pixel 9 Pro XL, Android 17, USB serial `46061FDAS000A4`, file-transfer mode, on the Dell's USB-C port, USB 3.2 Gen 1 (5 Gbit/s) per `mtp-rs devices` (`speed=Super`) and the report header.
- Host: Dell Latitude 3420, Intel Core i3-1115G4 (2 cores, 4 threads), Windows 11 Pro 25H2 (build 26200.9974). mtp-rs 0.38.1, release binaries built on another Windows machine from `d3d2e97` with a static C runtime and copied over, so the header says `(at build time)`. mtp-rs goes through the WPD backend; the phone is bound to Windows' own MTP driver.
- Host load: idle at 3–17% CPU before the run. Every chunk started at 6–13% busy and ran at 19–23% over the run. Spotify, qBittorrent, and Dell's background agents were running as the owner left them.
- Chunks 11:38–12:18, one binary, the commands in `../../README.md` § On Windows with `--serial 46061FDAS000A4`.
- Two reruns, the spoiled runs left out:
  - `r1.json` is the second run. The first was given `--sizes 1MB,10MB,100MB` unquoted, which PowerShell reads as numbers (`1MB` is 1,048,576), so it measured 1, 10, and 100 MiB. The README's PowerShell block now quotes the lists.
  - `r2.json` is the second run. The first started at 41% CPU busy; it gave 6.67 s against 6.52 s for the rerun.
- What doesn't compare with the Mac runs:
  - Different host and OS, so throughput differs for that reason alone (see the M1 and M3 notes on how much the host matters).
  - No libmtp, so no speedup column.
  - mtp-rs uploads go out in one piece through WPD (`Uploads:` line); the Mac run's went in windows.
  - `list_files` (4.0 ms here against 23.8 ms on the Mac) is answered by the driver, which caches object properties, not one round trip per object.
- Pacing costs more here than on the Mac: paced 1 GB 6.70 s against 6.15 s held open (+9%, Mac +1%), and the 4 GB `list_during` paced download 28.0 s against 24.7 s (+13%, Mac +2.4%), with a wider spread (21.8–29.0 s). Fixed 32 MiB windows cost +16% (Mac +9%), 8 MiB +31% (Mac +33%). Not investigated.
- Step 3 (`paced_with_calls`, 1 GiB random file in `Download/big.bin`, `--folder DCIM/Camera --small` a 4.8 MB photo `--thumb`, `--out` to the Dell's SSD, default 500 ms pacing), every result's SHA-256 matched the source:
  - `--mode held`: 6.22 s (164.7 MiB/s).
  - `--mode quiet`: 7.19 s (142.5 MiB/s), 14 windows.
  - `--every 8`: 8.66 s (118.2 MiB/s), 18 windows, two rounds of calls (listing 3 ms, photo 54 ms, thumbnail 40 ms).
  - `--every 1`: 9.19 s (111.4 MiB/s), 16 rounds; the calls took 1.16 s in all (listing 3 ms, photo 48 ms, thumbnail 21 ms on average).
  - `--cancel-loop 20`: no wedge; each listing after a mid-window cancel answered in 53–65 ms.
- Date check, Windows 11 (the earlier one in #67 was Windows 10): the Dell is on Central Europe Standard Time, the phone on Europe/Stockholm. `mtp-rs put` of files dated 2019-06-14 10:23 and 2019-01-15 10:23 local landed on the phone at 1560500580 and 1547544180 (`stat`), so 08:23Z and 09:23Z. `ls -l` and `ls --json` showed `2019-06-14T10:23:00` and `2019-01-15T10:23:00`; `get` of the file and of the folder set local mtimes of 08:23Z and 09:23Z.
- The phone was left as found: no `mtp-bench-tmp`, `big.bin`, or date-test folder in `/Download`; nothing written to `DCIM`. On the Dell only the three binaries are left in `C:\mtp-bench-67`.
