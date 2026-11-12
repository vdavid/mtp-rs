- Date:       2026-11-12 10:18 +01:00
- Host:       Apple M1 Max (MacBookPro18,2)
- Host OS:    macOS 27.0.1
- mtp-rs:     0.38.1 (8df56d43811b)
- libmtp:     1.1.23
- Device:     Google Pixel 9 Pro XL (MTP device version 1.0)
- USB link:   USB 3.2 Gen 1 (5 Gbit/s)
- Host load:  5.08 4.06 3.85 (10 cores) at start, 6.49 5.83 4.76 (10 cores) at end

| Operation | File Size | mtp-rs Median | mtp-rs Std Dev | mtp-rs Throughput | libmtp Median | libmtp Std Dev | libmtp Throughput | Speedup |
|-----------|-----------|---------------|----------------|-------------------|---------------|----------------|-------------------|---------|
| download_paced | 1.0 GB | 6.048s | 72.8ms | 165.34 MB/s | — | — | — | N/A |
| download_stream | 1.0 GB | 5.974s | 81.0ms | 167.41 MB/s | — | — | — | N/A |
| download_windowed_008MiB | 1.0 GB | 7.941s | 503.0ms | 125.93 MB/s | — | — | — | N/A |
| download_windowed_032MiB | 1.0 GB | 6.486s | 87.4ms | 154.18 MB/s | — | — | — | N/A |
| download | 1.0 MB | 24.0ms | 5.7ms | 41.67 MB/s | 25.3ms | 4.6ms | 39.56 MB/s | 1.05x |
| download | 10.0 MB | 84.3ms | 8.3ms | 118.63 MB/s | 89.7ms | 5.3ms | 111.47 MB/s | 1.06x |
| download | 100.0 MB | 679.0ms | 27.9ms | 147.28 MB/s | 836.9ms | 33.7ms | 119.48 MB/s | 1.23x |
| download | 1.0 GB | 5.967s | 132.2ms | 167.58 MB/s | 7.977s | 46.9ms | 125.37 MB/s | 1.34x |
| list_files | N/A | 23.8ms | 10.1ms | — | 33.9ms | 2.8ms | — | 1.42x |
| upload | 1.0 MB | 82.9ms | 10.2ms | 12.07 MB/s | 71.6ms | 6.5ms | 13.97 MB/s | 0.86x |
| upload | 10.0 MB | 151.2ms | 6.0ms | 66.12 MB/s | 153.2ms | 15.4ms | 65.27 MB/s | 1.01x |
| upload | 100.0 MB | 485.0ms | 31.1ms | 206.19 MB/s | 851.3ms | 38.4ms | 117.47 MB/s | 1.76x |
| upload | 1.0 GB | 4.203s | 324.8ms | 237.90 MB/s | 7.531s | 72.3ms | 132.78 MB/s | 1.79x |

Listing the storage root halfway through a download (mtp-rs only; medians unless noted):

| Pacing | File Size | Runs | Download Median | Download Throughput | Listing Median | Listing Max |
|--------|-----------|------|-----------------|---------------------|----------------|-------------|
| held open | 4.0 GB | 7 | 24.042s | 166.38 MB/s | 12.072s | 12.518s |
| default (~2 s windows) | 4.0 GB | 7 | 24.628s | 162.41 MB/s | 777.5ms | 1.068s |
| fixed 8 MiB windows | 4.0 GB | 7 | 33.373s | 119.86 MB/s | 57.6ms | 65.8ms |
| fixed 32 MiB windows | 4.0 GB | 7 | 25.028s | 159.82 MB/s | 111.1ms | 133.1ms |
