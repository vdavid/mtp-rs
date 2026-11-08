//! Download a big file in windows while an app keeps using the device, and time both (#61).
//!
//! The scenario from #61: a file manager copies a large file off the phone, and between windows
//! (whenever `FileDownload::holds_session()` is `false`) it lists a folder and loads a small photo,
//! the way a UI refreshes while a copy runs. On Windows WPD before the passthrough fix, every
//! window after such a call reopened the forward-only stream and re-read the file's prefix.
//!
//! Needs a real device. Read-only on the device; writes the downloaded file locally so you can
//! check its hash (`Get-FileHash` on Windows, `shasum -a 256` elsewhere).
//!
//! ```text
//! cargo run --release -p mtp-rs --example paced_with_calls -- Download/big.bin \
//!     --folder DCIM/Camera --small DCIM/Camera/IMG_1.jpg --out big.bin
//! ```
//!
//! Paths are `/`-separated from the first storage's root.
//!
//! Options:
//! - `--mode calls|quiet|held`: calls between windows (default), no calls, or one held-open read
//! - `--pacing-ms N`: the `MaxCancelDelay` target (default 500)
//! - `--folder PATH`: the folder to list between windows (default: the file's folder)
//! - `--small PATH`: a small file to read with `download_to_vec` between windows (optional)
//! - `--thumb`: also read the small file's thumbnail between windows
//! - `--every N`: run the calls between every Nth window only (default 1)
//! - `--out PATH`: write the downloaded bytes there
//! - `--cancel-loop N`: instead of a download, N rounds of: start a paced download at a spread of
//!   offsets, cancel it mid-window, then list the folder (fails on a wedge: a list over 30 s)

use mtp_rs::mtp::{MtpDevice, Storage};
use mtp_rs::{ByteRange, DownloadPacing, ObjectHandle, ObjectInfo, DEFAULT_CANCEL_TIMEOUT};
use std::io::Write;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: paced_with_calls FILE/PATH [--mode calls|quiet|held] [--pacing-ms N] \
    [--folder PATH] [--small PATH] [--thumb] [--every N] [--out PATH] [--cancel-loop N]";

#[derive(PartialEq)]
enum Mode {
    Calls,
    Quiet,
    Held,
}

struct Opts {
    file: String,
    mode: Mode,
    pacing: Duration,
    folder: Option<String>,
    small: Option<String>,
    thumb: bool,
    every: u32,
    out: Option<String>,
    cancel_loop: u32,
}

fn parse() -> Result<Opts, Box<dyn std::error::Error>> {
    let mut o = Opts {
        file: String::new(),
        mode: Mode::Calls,
        pacing: Duration::from_millis(500),
        folder: None,
        small: None,
        thumb: false,
        every: 1,
        out: None,
        cancel_loop: 0,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or(USAGE);
        match a.as_str() {
            "--mode" => {
                o.mode = match val()?.as_str() {
                    "calls" => Mode::Calls,
                    "quiet" => Mode::Quiet,
                    "held" => Mode::Held,
                    _ => return Err(USAGE.into()),
                }
            }
            "--pacing-ms" => o.pacing = Duration::from_millis(val()?.parse()?),
            "--folder" => o.folder = Some(val()?),
            "--small" => o.small = Some(val()?),
            "--thumb" => o.thumb = true,
            "--every" => o.every = val()?.parse::<u32>()?.max(1),
            "--out" => o.out = Some(val()?),
            "--cancel-loop" => o.cancel_loop = val()?.parse()?,
            s if s.starts_with("--") => return Err(USAGE.into()),
            s => o.file = s.trim_matches('/').to_string(),
        }
    }
    if o.file.is_empty() {
        return Err(USAGE.into());
    }
    Ok(o)
}

/// Resolve a `/`-separated path from the storage root, by name, one listing per level.
async fn resolve(storage: &Storage, path: &str) -> Result<ObjectInfo, Box<dyn std::error::Error>> {
    let mut parent: Option<ObjectHandle> = None;
    let mut found = None;
    for part in path.split('/').filter(|s| !s.is_empty()) {
        let kids = storage.list_objects(parent).await?;
        let hit = kids
            .into_iter()
            .find(|o| o.filename.eq_ignore_ascii_case(part))
            .ok_or_else(|| format!("'{part}' not found in {path}"))?;
        parent = Some(hit.handle);
        found = Some(hit);
    }
    found.ok_or_else(|| "empty path".into())
}

fn mib_s(bytes: u64, d: Duration) -> f64 {
    bytes as f64 / (1024.0 * 1024.0) / d.as_secs_f64().max(1e-9)
}

#[derive(Default)]
struct CallStats {
    count: u32,
    total: Duration,
    max: Duration,
}

impl CallStats {
    fn add(&mut self, d: Duration) {
        self.count += 1;
        self.total += d;
        self.max = self.max.max(d);
    }

    fn line(&self, name: &str) -> String {
        if self.count == 0 {
            return format!("{name}: none");
        }
        format!(
            "{name}: {} calls, avg {:.1} ms, max {:.1} ms, total {:.2} s",
            self.count,
            self.total.as_secs_f64() * 1000.0 / f64::from(self.count),
            self.max.as_secs_f64() * 1000.0,
            self.total.as_secs_f64()
        )
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let o = parse()?;
    let t_open = Instant::now();
    let device = MtpDevice::open_first().await?;
    println!(
        "Opened {} {} in {:.0} ms",
        device.device_info().manufacturer,
        device.device_info().model,
        t_open.elapsed().as_secs_f64() * 1000.0
    );
    let storages = device.storages().await?;
    let storage = storages.first().ok_or("no storage")?;

    let file = resolve(storage, &o.file).await?;
    let folder_path = o
        .folder
        .clone()
        .unwrap_or_else(|| match o.file.rsplit_once('/') {
            Some((dir, _)) => dir.to_string(),
            None => String::new(),
        });
    let folder = if folder_path.is_empty() {
        None
    } else {
        Some(resolve(storage, &folder_path).await?.handle)
    };
    let small = match &o.small {
        Some(p) => Some(resolve(storage, p).await?),
        None => None,
    };
    println!(
        "File: {} ({} bytes, {:.1} MiB)",
        file.filename,
        file.size,
        file.size as f64 / (1024.0 * 1024.0)
    );

    if o.cancel_loop > 0 {
        return cancel_loop(storage, &file, folder, o.pacing, o.cancel_loop).await;
    }

    let pacing = match o.mode {
        Mode::Held => DownloadPacing::HeldOpen,
        _ => DownloadPacing::MaxCancelDelay(o.pacing),
    };
    let mut out = match &o.out {
        Some(p) => Some(std::io::BufWriter::new(std::fs::File::create(p)?)),
        None => None,
    };
    let mut lists = CallStats::default();
    let mut smalls = CallStats::default();
    let mut thumbs = CallStats::default();
    let mut windows = 0u32;
    let mut boundaries: Vec<(u64, Duration)> = Vec::new();
    let mut received = 0u64;
    let mut last_report = Instant::now();

    let t0 = Instant::now();
    let mut download = storage
        .download_with_pacing(file.handle, ByteRange::Full, pacing)
        .await?;
    while let Some(chunk) = download.next_chunk().await {
        let chunk = chunk?;
        received += chunk.len() as u64;
        if let Some(w) = out.as_mut() {
            w.write_all(&chunk)?;
        }
        if last_report.elapsed() > Duration::from_secs(5) {
            println!(
                "  {:.0} MiB at {:.1} MiB/s",
                received as f64 / (1024.0 * 1024.0),
                mib_s(received, t0.elapsed())
            );
            last_report = Instant::now();
        }
        if download.holds_session() || received == file.size {
            continue;
        }
        windows += 1;
        boundaries.push((received, t0.elapsed()));
        if o.mode != Mode::Calls || !windows.is_multiple_of(o.every) {
            continue;
        }
        let t = Instant::now();
        storage.list_objects(folder).await?;
        lists.add(t.elapsed());
        if let Some(s) = &small {
            let t = Instant::now();
            storage.download_to_vec(s.handle).await?;
            smalls.add(t.elapsed());
            if o.thumb {
                let t = Instant::now();
                // Not every file has a thumbnail; time the call either way.
                let _ = storage.thumbnail(s.handle).await;
                thumbs.add(t.elapsed());
            }
        }
    }
    let total = t0.elapsed();
    if let Some(mut w) = out {
        w.flush()?;
    }
    let calls = lists.total + smalls.total + thumbs.total;
    println!(
        "\nDownloaded {received} of {} bytes in {:.2} s: {:.1} MiB/s overall, {:.1} MiB/s without the calls' own time",
        file.size,
        total.as_secs_f64(),
        mib_s(received, total),
        mib_s(received, total.saturating_sub(calls))
    );
    println!("Window boundaries seen: {windows}");
    // Each window's size and how long it took from the end of the previous one (calls included).
    let mut prev = (0u64, Duration::ZERO);
    let sizes: Vec<String> = boundaries
        .iter()
        .map(|&(at, when)| {
            let s = format!(
                "{:.0}M/{:.0}ms",
                (at - prev.0) as f64 / (1024.0 * 1024.0),
                (when - prev.1).as_secs_f64() * 1000.0
            );
            prev = (at, when);
            s
        })
        .collect();
    println!(
        "Windows (size/time since the last boundary): {}",
        sizes.join(" ")
    );
    println!("{}", lists.line("list_objects"));
    println!("{}", smalls.line("download_to_vec (small)"));
    println!("{}", thumbs.line("thumbnail"));
    if received != file.size {
        return Err("short download".into());
    }
    Ok(())
}

async fn cancel_loop(
    storage: &Storage,
    file: &ObjectInfo,
    folder: Option<ObjectHandle>,
    pacing: Duration,
    rounds: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\nCancel loop: {rounds} rounds");
    for i in 0..rounds {
        // Spread the start over the file, so some cancels land far in.
        let start = file.size / u64::from(rounds) * u64::from(i);
        let mut download = storage
            .download_with_pacing(
                file.handle,
                ByteRange::From(start),
                DownloadPacing::MaxCancelDelay(pacing),
            )
            .await?;
        // Read a few chunks, so the cancel lands mid-window.
        let mut got = 0u64;
        for _ in 0..(3 + i % 5) {
            match download.next_chunk().await {
                Some(c) => got += c?.len() as u64,
                None => break,
            }
        }
        let mid_window = download.holds_session();
        let t = Instant::now();
        download.cancel(DEFAULT_CANCEL_TIMEOUT).await?;
        let cancelled = t.elapsed();
        let t = Instant::now();
        let listed =
            match tokio::time::timeout(Duration::from_secs(30), storage.list_objects(folder)).await
            {
                Ok(r) => r?,
                Err(_) => {
                    return Err(format!("round {i}: listing after the cancel hung (wedge)").into())
                }
            };
        println!(
            "  round {i:>2}: from {start}, read {got} B, mid-window {mid_window}, cancel {:.0} ms, then listed {} objects in {:.0} ms",
            cancelled.as_secs_f64() * 1000.0,
            listed.len(),
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    println!("No wedge in {rounds} rounds.");
    Ok(())
}
