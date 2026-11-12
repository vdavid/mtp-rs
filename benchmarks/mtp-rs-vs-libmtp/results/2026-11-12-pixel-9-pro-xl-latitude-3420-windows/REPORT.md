- Date:       2026-11-12 12:17 +01:00
- Host:       11th Gen Intel(R) Core(TM) i3-1115G4 @ 3.00GHz (Dell Inc. Latitude 3420)
- Host OS:    Windows 11 Pro 25H2 (build 26200.9974)
- mtp-rs:     0.38.1 (d3d2e97787c7 (at build time))
- libmtp:     not in this build (libmtp isn't built on Windows)
- Backends:   mtp-rs (no libmtp: libmtp isn't built on Windows)
- Device:     Google Pixel 9 Pro XL (MTP device version 1.0)
- USB link:   USB 3.2 Gen 1 (5 Gbit/s)
- Uploads:    mtp-rs in one piece (no windowed uploads on this device or backend)
- Host load:  CPU 13% busy over 1 s (4 cores) at start, CPU 10% busy over 1 s, 19% over the run (4 cores) at end

| Operation | File Size | mtp-rs Median | mtp-rs Std Dev | mtp-rs Throughput | libmtp Median | libmtp Std Dev | libmtp Throughput | Speedup |
|-----------|-----------|---------------|----------------|-------------------|---------------|----------------|-------------------|---------|
| download_paced | 1.0 GB | 6.701s | 81.1ms | 149.22 MB/s | — | — | — | N/A |
| download_stream | 1.0 GB | 6.147s | 195.3ms | 162.69 MB/s | — | — | — | N/A |
| download_windowed_008MiB | 1.0 GB | 8.080s | 292.3ms | 123.77 MB/s | — | — | — | N/A |
| download_windowed_032MiB | 1.0 GB | 7.144s | 280.2ms | 139.98 MB/s | — | — | — | N/A |
| download | 1.0 MB | 22.3ms | 2.9ms | 44.77 MB/s | — | — | — | N/A |
| download | 10.0 MB | 93.1ms | 3.5ms | 107.38 MB/s | — | — | — | N/A |
| download | 100.0 MB | 705.4ms | 214.1ms | 141.76 MB/s | — | — | — | N/A |
| download | 1.0 GB | 6.521s | 205.1ms | 153.35 MB/s | — | — | — | N/A |
| list_files | N/A | 4.0ms | 0.3ms | — | — | — | — | N/A |
| upload | 1.0 MB | 70.8ms | 5.5ms | 14.12 MB/s | — | — | — | N/A |
| upload | 10.0 MB | 126.3ms | 7.5ms | 79.15 MB/s | — | — | — | N/A |
| upload | 100.0 MB | 567.3ms | 47.6ms | 176.29 MB/s | — | — | — | N/A |
| upload | 1.0 GB | 4.760s | 170.9ms | 210.09 MB/s | — | — | — | N/A |

Listing the storage root halfway through a download (mtp-rs only; medians unless noted):

| Pacing | File Size | Runs | Download Median | Download Throughput | Listing Median | Listing Max |
|--------|-----------|------|-----------------|---------------------|----------------|-------------|
| held open | 4.0 GB | 7 | 24.694s | 161.98 MB/s | 12.090s | 12.492s |
| default (~2 s windows) | 4.0 GB | 7 | 27.964s | 143.04 MB/s | 1.223s | 1.875s |
| fixed 8 MiB windows | 4.0 GB | 7 | 35.697s | 112.06 MB/s | 42.0ms | 43.7ms |
| fixed 32 MiB windows | 4.0 GB | 7 | 29.835s | 134.07 MB/s | 102.2ms | 104.1ms |
