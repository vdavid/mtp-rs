//! Hardware probe for paced uploads (#62): can an upload to an Android phone go out as a series of
//! `SendPartialObject` windows inside a `BeginEditObject`/`EndEditObject` edit, so the session is
//! free between windows the way paced downloads free it?
//!
//! It drives `ptp::PtpSession` directly, so it tests the phone, not a `UsbBackend` upload path
//! (there isn't one yet). What each command answers, and the results, are in
//! `docs/specs/paced-uploads.md`.
//!
//! **Writes to the phone.** Every object it creates is named `mtprs-probe-*` and lives in the
//! first storage's `Download` folder; each command deletes what it made, and `cleanup` deletes any
//! leftovers. Android only: the edit operations are an AOSP extension.
//!
//! Run: `cargo run --release -p mtp-rs --example android_edit_probe -- <command> [args]`
//!
//! Commands:
//! - `info`: the vendor extension and whether `DeviceInfo` lists 0x95C1–0x95C5
//! - `combos`: how `SendObjectInfo`, `SendObject`, and the edit ops combine, on a 3 MiB object
//! - `bench <file> <window>...`: one held-open `SendObject` of `<file>`, then one windowed upload
//!   per window size (`1M`, `8M`, `128M`, ...), each read back and compared byte for byte
//! - `interleave <file> <window|held>`: an upload while a second task lists `Download`, reads an
//!   `ObjectInfo`, and downloads a small file every 250 ms, with each call's latency
//! - `stop <noend-close|noend-drop|cancel|drop|exit> <file> <window> [runs]`: stop an upload
//!   partway and look at what's left and whether the session still works
//! - `check`: open a fresh session and describe every `mtprs-probe-*` object (after `stop exit`)
//! - `cleanup`: delete every `mtprs-probe-*` object in `Download`
//!
//! Options (anywhere after the command): `--ext EXT` (file extension, default `bin`),
//! `--check-every N` (`GetObjectInfo` on the handle every N windows, default 0 = only at the end).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::stream::{self, Stream, StreamExt};
use mtp_rs::ptp::{
    DateTime, ObjectFormatCode, ObjectHandle, ObjectInfo, OperationCode, PtpDevice, PtpError,
    PtpSession, StorageId,
};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const CHUNK: usize = 1024 * 1024;
const PREFIX: &str = "mtprs-probe-";

struct Ctx {
    // Kept so the USB interface stays claimed for as long as the session lives.
    _device: PtpDevice,
    session: Arc<PtpSession>,
    storage: StorageId,
    download: ObjectHandle,
    ext: String,
    check_every: usize,
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(args).await {
        eprintln!("\nprobe failed: {e}");
        std::process::exit(1);
    }
}

async fn run(mut args: Vec<String>) -> R<()> {
    let ext = take_opt(&mut args, "--ext").unwrap_or_else(|| "bin".into());
    let check_every = take_opt(&mut args, "--check-every")
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(0);
    let cmd = args.first().cloned().unwrap_or_default();
    let started = Instant::now();
    let mut ctx = open(ext, check_every).await?;
    println!("open: {:.2?}", started.elapsed());
    match cmd.as_str() {
        "info" => info(&ctx).await?,
        "combos" => combos(&ctx).await?,
        "bench" => {
            let file = PathBuf::from(args.get(1).ok_or("bench <file> <window>...")?);
            let windows = args[2..]
                .iter()
                .map(|s| parse_size(s))
                .collect::<R<Vec<_>>>()?;
            bench(&ctx, &file, &windows).await?;
        }
        "interleave" => {
            let file = PathBuf::from(args.get(1).ok_or("interleave <file> <window|held>")?);
            let window = match args.get(2).map(String::as_str) {
                Some("held") | None => None,
                Some(s) => Some(parse_size(s)?),
            };
            interleave(&ctx, &file, window).await?;
        }
        "stop" => {
            let mode = args
                .get(1)
                .ok_or("stop <mode> <file> <window> [runs]")?
                .clone();
            let file = PathBuf::from(args.get(2).ok_or("stop <mode> <file> <window>")?);
            let window = parse_size(args.get(3).ok_or("stop <mode> <file> <window>")?)?;
            let runs: usize = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(1);
            for run in 1..=runs {
                println!("\n=== stop {mode}, run {run}/{runs} ===");
                ctx = stop(ctx, &mode, &file, window).await?;
            }
        }
        "check" => check(&ctx).await?,
        "cleanup" => cleanup(&ctx).await?,
        _ => {
            return Err(
                "command: info | combos | bench | interleave | stop | check | cleanup".into(),
            )
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

async fn info(ctx: &Ctx) -> R<()> {
    let info = ctx.session.get_device_info().await?;
    println!(
        "{} {} ({})",
        info.manufacturer, info.model, info.device_version
    );
    println!("vendor extension: {:?}", info.vendor_extension_desc);
    for code in 0x95C1u16..=0x95C5 {
        let listed = info
            .operations_supported
            .iter()
            .any(|&op| u16::from(op) == code);
        println!("  0x{code:04X} {:?}: {}", OperationCode::from(code), listed);
    }
    let vendor: Vec<String> = info
        .operations_supported
        .iter()
        .map(|&op| u16::from(op))
        .filter(|&c| c >= 0x9000)
        .map(|c| format!("0x{c:04X}"))
        .collect();
    println!("all ops >= 0x9000: {}", vendor.join(" "));
    Ok(())
}

/// Each case runs on a fresh object and ends by reading it back and deleting it.
async fn combos(ctx: &Ctx) -> R<()> {
    let len: usize = 3 * CHUNK;
    let data: Vec<u8> = (0..len).map(|i| pattern(i as u64)).collect();
    let data = Bytes::from(data);
    let s = &ctx.session;

    // (name, declared size, send an empty SendObject first)
    let cases: [(&str, u64, bool); 4] = [
        ("A: SOI(size=N), no SendObject, edit", len as u64, false),
        ("B: SOI(size=0), empty SendObject, edit", 0, true),
        ("C: SOI(size=N), empty SendObject, edit", len as u64, true),
        ("D: SOI(size=0), no SendObject, edit", 0, false),
    ];
    for (i, (name, declared, empty_send)) in cases.into_iter().enumerate() {
        println!("\n--- {name}");
        let filename = format!("{PREFIX}combo-{}.{}", (b'a' + i as u8) as char, ctx.ext);
        let t = Instant::now();
        let handle = match send_info(ctx, &filename, declared).await {
            Ok(h) => h,
            Err(e) => {
                println!("  SendObjectInfo: {e}");
                continue;
            }
        };
        println!("  SendObjectInfo -> {:?} ({:.1?})", handle, t.elapsed());
        // No other operation here: AOSP forgets a pending SendObjectInfo on any command other
        // than SendObject (`MtpServer::handleRequest`).
        if empty_send {
            let t = Instant::now();
            let r = s
                .send_object_stream(0, stream::iter(Vec::<std::io::Result<Bytes>>::new()))
                .await;
            println!("  SendObject(0 bytes): {} ({:.1?})", show(&r), t.elapsed());
            describe(ctx, handle, "  after SendObject").await;
        }
        let t = Instant::now();
        let r = s.begin_edit_object(handle).await;
        println!("  BeginEditObject: {} ({:.1?})", show(&r), t.elapsed());
        if r.is_ok() {
            let mut offset = 0u64;
            for w in 0..3 {
                let chunk = data.slice(w * CHUNK..(w + 1) * CHUNK);
                let t = Instant::now();
                let r = s
                    .send_partial_object(handle, offset, CHUNK as u32, one(chunk))
                    .await;
                println!(
                    "  SendPartialObject(off={offset}, len={CHUNK}): {} ({:.1?})",
                    show(&r),
                    t.elapsed()
                );
                if r.is_err() {
                    break;
                }
                offset += CHUNK as u64;
            }
            describe(ctx, handle, "  mid-edit").await;
            if i == 1 {
                edge_cases(ctx, handle, offset, &data).await;
            }
            let t = Instant::now();
            let r = s.end_edit_object(handle).await;
            println!("  EndEditObject: {} ({:.1?})", show(&r), t.elapsed());
        }
        settle_and_verify(ctx, handle, &filename, &data).await;
        delete(ctx, handle).await;
    }
    Ok(())
}

/// Run on case B's open edit, at `size` bytes written.
async fn edge_cases(ctx: &Ctx, handle: ObjectHandle, size: u64, data: &Bytes) {
    let s = &ctx.session;
    let r = s.begin_edit_object(handle).await;
    println!("  edge: second BeginEditObject: {}", show(&r));
    let r = s
        .send_partial_object(handle, size + 1, 1, one(Bytes::from_static(b"x")))
        .await;
    println!("  edge: SendPartialObject one past the end: {}", show(&r));
    let r = s
        .send_partial_object(handle, 0, 1, one(data.slice(0..1)))
        .await;
    println!(
        "  edge: SendPartialObject rewriting byte 0 (same value): {}",
        show(&r)
    );
    let r = s.truncate_object(handle, size - 10).await;
    println!("  edge: TruncateObject(size - 10): {}", show(&r));
    describe(ctx, handle, "  edge: after truncate").await;
    let tail = data.slice(size as usize - 10..size as usize);
    let r = s
        .send_partial_object(handle, size - 10, 10, one(tail))
        .await;
    println!(
        "  edge: SendPartialObject restoring the last 10 bytes: {}",
        show(&r)
    );
}

async fn bench(ctx: &Ctx, file: &Path, windows: &[u64]) -> R<()> {
    let size = std::fs::metadata(file)?.len();
    println!("file: {} ({} bytes)", file.display(), size);

    let name = format!("{PREFIX}held.{}", ctx.ext);
    let t = Instant::now();
    let h = send_info(ctx, &name, size).await?;
    let soi = t.elapsed();
    let t = Instant::now();
    ctx.session
        .send_object_stream(size, file_stream(file, 0, size)?)
        .await?;
    let held = t.elapsed();
    println!(
        "held-open: SOI {:.1?}, SendObject {:.2?} = {:.1} MB/s",
        soi,
        held,
        mbps(size, held)
    );
    verify(ctx, h, file, size).await?;
    delete(ctx, h).await;

    for &window in windows {
        let name = format!("{PREFIX}win-{}.{}", human(window), ctx.ext);
        let report = upload_windowed(ctx, &name, file, size, window, None).await?;
        let per = report
            .windows
            .iter()
            .map(|w| w.as_secs_f64())
            .collect::<Vec<_>>();
        let data_time: f64 = per.iter().sum();
        let ideal = held.as_secs_f64() * (window.min(size) as f64 / size as f64);
        let overhead_ms = (data_time - held.as_secs_f64()) / per.len() as f64 * 1000.0;
        println!(
            "window {:>5}: {} windows, total {:.2?} = {:.1} MB/s; setup {:.1?}, end-edit {:.1?}; \
             per window median {:.1} ms, min {:.1}, max {:.1} (held-open share {:.1} ms) -> \
             ~{:.1} ms overhead per window",
            human(window),
            per.len(),
            report.total,
            mbps(size, report.total),
            report.setup,
            report.end,
            median(&per) * 1000.0,
            per.iter().cloned().fold(f64::MAX, f64::min) * 1000.0,
            per.iter().cloned().fold(0.0, f64::max) * 1000.0,
            ideal * 1000.0,
            overhead_ms,
        );
        verify(ctx, report.handle, file, size).await?;
        delete(ctx, report.handle).await;
    }
    Ok(())
}

async fn interleave(ctx: &Ctx, file: &Path, window: Option<u64>) -> R<()> {
    let size = std::fs::metadata(file)?.len();
    // A small file for the download, and any object for GetObjectInfo: the smallest non-probe
    // file in Download.
    let handles = ctx
        .session
        .get_object_handles(ctx.storage, None, Some(ctx.download))
        .await?;
    let mut small: Option<(ObjectHandle, u64, String)> = None;
    for h in handles.iter().take(200) {
        let info = ctx.session.get_object_info(*h).await?;
        if info.format != ObjectFormatCode::Association
            && !info.filename.starts_with(PREFIX)
            && info.size > 10_000
            && small.as_ref().is_none_or(|s| info.size < s.1)
        {
            small = Some((*h, info.size, info.filename));
        }
    }
    let (small, small_size, small_name) = small.ok_or("no small file in Download")?;
    println!("caller: lists Download ({} entries), GetObjectInfo + GetObject on {small_name:?} ({small_size} bytes)", handles.len());

    let baseline = caller_round(ctx, small).await?;
    println!(
        "baseline, idle session: list {:.1?}, info {:.1?}, download {:.1?}",
        baseline[0], baseline[1], baseline[2]
    );

    let done = Arc::new(AtomicBool::new(false));
    let caller = {
        let session = Arc::clone(&ctx.session);
        let (storage, download) = (ctx.storage, ctx.download);
        let done = Arc::clone(&done);
        tokio::spawn(async move {
            let mut rounds: Vec<[Duration; 3]> = Vec::new();
            while !done.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let mut r = [Duration::ZERO; 3];
                let t = Instant::now();
                if session
                    .get_object_handles(storage, None, Some(download))
                    .await
                    .is_err()
                {
                    break;
                }
                r[0] = t.elapsed();
                let t = Instant::now();
                if session.get_object_info(small).await.is_err() {
                    break;
                }
                r[1] = t.elapsed();
                let t = Instant::now();
                if session.get_object(small).await.is_err() {
                    break;
                }
                r[2] = t.elapsed();
                rounds.push(r);
            }
            rounds
        })
    };

    let name = format!("{PREFIX}inter.{}", ctx.ext);
    let t = Instant::now();
    let handle = match window {
        Some(w) => {
            upload_windowed(ctx, &name, file, size, w, None)
                .await?
                .handle
        }
        None => {
            let h = send_info(ctx, &name, size).await?;
            ctx.session
                .send_object_stream(size, file_stream(file, 0, size)?)
                .await?;
            h
        }
    };
    let upload = t.elapsed();
    done.store(true, Ordering::Relaxed);
    let rounds = caller.await?;
    println!(
        "upload ({}): {:.2?} = {:.1} MB/s, {} caller rounds during it",
        window.map_or("held-open".into(), human),
        upload,
        mbps(size, upload),
        rounds.len()
    );
    for (i, label) in ["list", "info", "download"].iter().enumerate() {
        let v: Vec<f64> = rounds.iter().map(|r| r[i].as_secs_f64() * 1000.0).collect();
        if v.is_empty() {
            continue;
        }
        println!(
            "  {label:>8}: median {:.1} ms, p90 {:.1} ms, max {:.1} ms",
            median(&v),
            percentile(&v, 0.9),
            v.iter().cloned().fold(0.0, f64::max)
        );
    }
    verify(ctx, handle, file, size).await?;
    delete(ctx, handle).await;
    Ok(())
}

async fn stop(ctx: Ctx, mode: &str, file: &Path, window: u64) -> R<Ctx> {
    let size = std::fs::metadata(file)?.len();
    let name = format!("{PREFIX}stop-{mode}.{}", ctx.ext);
    let s = Arc::clone(&ctx.session);
    let handle = begin_upload(&ctx, &name).await?;
    // Two whole windows, then stop inside the third.
    for w in 0..2u64 {
        s.send_partial_object(
            handle,
            w * window,
            window as u32,
            file_stream(file, w * window, window)?,
        )
        .await?;
    }
    let offset = 2 * window;
    let stop_after = window / 2;
    describe(&ctx, handle, "after two windows").await;

    match mode {
        "noend-close" | "noend-drop" => {
            let Ctx {
                _device, session, ..
            } = ctx;
            if mode == "noend-close" {
                let session = Arc::try_unwrap(session).map_err(|_| "session still shared")?;
                let t = Instant::now();
                println!(
                    "CloseSession: {} ({:.1?})",
                    show(&session.close().await),
                    t.elapsed()
                );
            } else {
                drop(session);
            }
            drop(_device);
            tokio::time::sleep(Duration::from_secs(1)).await;
            let t = Instant::now();
            let ctx = open(ctx_ext(&name), 0).await?;
            println!("reopen: {:.1?}", t.elapsed());
            let h = find(&ctx, &name).await?;
            println!("object after reopen: {:?} (was {:?})", h, handle);
            if let Some(h) = h {
                describe(&ctx, h, "after reopen").await;
                let r = ctx.session.begin_edit_object(h).await;
                println!("BeginEditObject again: {}", show(&r));
                if r.is_ok() {
                    let r = ctx.session.end_edit_object(h).await;
                    println!("EndEditObject: {}", show(&r));
                }
                verify_prefix(&ctx, h, file).await;
                delete(&ctx, h).await;
            }
            Ok(ctx)
        }
        "cancel" => {
            // The source fails halfway through the window, the way a progress Break does.
            let sent = Arc::new(AtomicU64::new(0));
            let src = failing_after(
                file_stream(file, offset, window)?,
                stop_after,
                Arc::clone(&sent),
            );
            let t = Instant::now();
            let r = s
                .send_partial_object(handle, offset, window as u32, src)
                .await;
            println!(
                "SendPartialObject stopped after {} bytes: {} ({:.1?})",
                sent.load(Ordering::Relaxed),
                show(&r),
                t.elapsed()
            );
            after_stop(&ctx, handle, file, offset).await;
            Ok(ctx)
        }
        "drop" => {
            let fut = s.send_partial_object(
                handle,
                offset,
                window as u32,
                file_stream(file, offset, window)?,
            );
            // A 32 MiB window takes ~150-250 ms; stop about halfway.
            let r = tokio::time::timeout(Duration::from_millis(100), fut).await;
            println!(
                "SendPartialObject future dropped: timed out = {}",
                r.is_err()
            );
            after_stop(&ctx, handle, file, offset).await;
            Ok(ctx)
        }
        "exit" => {
            let s2 = Arc::clone(&s);
            let src = file_stream(file, offset, window)?;
            tokio::spawn(async move {
                let _ = s2
                    .send_partial_object(handle, offset, window as u32, src)
                    .await;
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            println!("exiting mid-window (handle {handle:?}); run `check` next");
            std::process::exit(0);
        }
        _ => Err(format!("unknown stop mode {mode} (file size {size})").into()),
    }
}

/// What's left after a stop mid-window, and whether the edit and the session still work.
async fn after_stop(ctx: &Ctx, handle: ObjectHandle, file: &Path, offset: u64) {
    let s = &ctx.session;
    let t = Instant::now();
    let r = s.get_storage_ids().await;
    println!(
        "next op (GetStorageIds): {} ({:.1?})",
        show(&r),
        t.elapsed()
    );
    let t = Instant::now();
    let r = s
        .get_object_handles(ctx.storage, None, Some(ctx.download))
        .await;
    println!(
        "listing: {} ({:.1?})",
        show(&r.map(|v| v.len())),
        t.elapsed()
    );
    describe(ctx, handle, "after stop").await;
    // Is the edit still open? Rewrite the stopped window from its start.
    let window = 4 * CHUNK as u64;
    let r = match file_stream(file, offset, window) {
        Ok(src) => {
            s.send_partial_object(handle, offset, window as u32, src)
                .await
        }
        Err(e) => {
            println!("file read failed: {e}");
            return;
        }
    };
    println!(
        "SendPartialObject resuming at the stopped window's offset: {}",
        show(&r)
    );
    let r = s.end_edit_object(handle).await;
    println!("EndEditObject: {}", show(&r));
    describe(ctx, handle, "after end").await;
    verify_prefix(ctx, handle, file).await;
    delete(ctx, handle).await;
}

async fn check(ctx: &Ctx) -> R<()> {
    let handles = ctx
        .session
        .get_object_handles(ctx.storage, None, Some(ctx.download))
        .await?;
    for h in handles {
        let info = ctx.session.get_object_info(h).await?;
        if info.filename.starts_with(PREFIX) {
            println!(
                "{:?} {:?} size {} modified {:?}",
                h, info.filename, info.size, info.modified
            );
            let r = ctx.session.begin_edit_object(h).await;
            println!("  BeginEditObject: {}", show(&r));
            if r.is_ok() {
                println!(
                    "  EndEditObject: {}",
                    show(&ctx.session.end_edit_object(h).await)
                );
            }
        }
    }
    Ok(())
}

async fn cleanup(ctx: &Ctx) -> R<()> {
    let handles = ctx
        .session
        .get_object_handles(ctx.storage, None, Some(ctx.download))
        .await?;
    for h in handles {
        let info = ctx.session.get_object_info(h).await?;
        if info.filename.starts_with(PREFIX) {
            println!("deleting {:?} ({} bytes)", info.filename, info.size);
            delete(ctx, h).await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Upload shapes
// ---------------------------------------------------------------------------------------------

struct Windowed {
    handle: ObjectHandle,
    setup: Duration,
    windows: Vec<Duration>,
    end: Duration,
    total: Duration,
}

/// `SendObjectInfo(size=0)`, an empty `SendObject`, `BeginEditObject`: the shape `combos` found
/// to work.
async fn begin_upload(ctx: &Ctx, name: &str) -> R<ObjectHandle> {
    let h = send_info(ctx, name, 0).await?;
    ctx.session
        .send_object_stream(0, stream::iter(Vec::<std::io::Result<Bytes>>::new()))
        .await?;
    ctx.session.begin_edit_object(h).await?;
    Ok(h)
}

async fn upload_windowed(
    ctx: &Ctx,
    name: &str,
    file: &Path,
    size: u64,
    window: u64,
    _unused: Option<()>,
) -> R<Windowed> {
    let total = Instant::now();
    let handle = begin_upload(ctx, name).await?;
    let setup = total.elapsed();
    let mut windows = Vec::new();
    let mut offset = 0;
    let mut n = 0usize;
    while offset < size {
        let len = window.min(size - offset);
        let src = file_stream(file, offset, len)?;
        let t = Instant::now();
        ctx.session
            .send_partial_object(handle, offset, len as u32, src)
            .await?;
        windows.push(t.elapsed());
        offset += len;
        n += 1;
        // Let a caller waiting on the session lock in, as the download pacer does.
        tokio::task::yield_now().await;
        if ctx.check_every > 0 && n.is_multiple_of(ctx.check_every) {
            match ctx.session.get_object_info(handle).await {
                Ok(i) if i.size == offset => {}
                Ok(i) => println!(
                    "  window {n}: handle answers, size {} (written {offset})",
                    i.size
                ),
                Err(e) => println!("  window {n}: handle {handle:?} no longer answers: {e}"),
            }
        }
    }
    let t = Instant::now();
    ctx.session.end_edit_object(handle).await?;
    let end = t.elapsed();
    Ok(Windowed {
        handle,
        setup,
        windows,
        end,
        total: total.elapsed(),
    })
}

async fn send_info(ctx: &Ctx, name: &str, size: u64) -> Result<ObjectHandle, PtpError> {
    let mut info = ObjectInfo::default();
    info.storage_id = ctx.storage;
    info.format = ObjectFormatCode::Undefined;
    info.size = size;
    info.filename = name.to_string();
    // A date far from now, to see whether the edit path keeps it.
    info.modified = DateTime::new(2020, 1, 2, 3, 4, 5);
    let (_, _, h) = ctx
        .session
        .send_object_info(ctx.storage, ctx.download, &info)
        .await?;
    Ok(h)
}

// ---------------------------------------------------------------------------------------------
// Reading back
// ---------------------------------------------------------------------------------------------

async fn describe(ctx: &Ctx, handle: ObjectHandle, label: &str) {
    match ctx.session.get_object_info(handle).await {
        Ok(i) => println!(
            "{label}: {:?} size {} modified {}",
            i.filename,
            i.size,
            i.modified.map_or("none".into(), fmt_date)
        ),
        Err(e) => println!("{label}: GetObjectInfo({handle:?}) failed: {e}"),
    }
}

/// After the edit: is the handle the one a fresh listing shows, and do the bytes match?
async fn settle_and_verify(ctx: &Ctx, handle: ObjectHandle, name: &str, data: &Bytes) {
    describe(ctx, handle, "  after end").await;
    match find(ctx, name).await {
        Ok(found) => println!("  listing shows {:?} (handle {:?})", found, handle),
        Err(e) => println!("  listing failed: {e}"),
    }
    match ctx.session.get_object(handle).await {
        Ok(got) if got == data[..] => println!("  content: matches ({} bytes)", got.len()),
        Ok(got) => println!(
            "  content: DIFFERS ({} bytes, first difference at {:?})",
            got.len(),
            got.iter().zip(data.iter()).position(|(a, b)| a != b)
        ),
        Err(e) => println!("  content: GetObject failed: {e}"),
    }
}

/// Stream the object back and compare it to the file.
async fn verify(ctx: &Ctx, handle: ObjectHandle, file: &Path, size: u64) -> R<()> {
    let info = ctx.session.get_object_info(handle).await?;
    let t = Instant::now();
    let (matched, got) = compare(ctx, handle, file).await?;
    println!(
        "  read back: size {} (want {size}), modified {}, content {} ({got} bytes, {:.1?})",
        info.size,
        info.modified.map_or("none".into(), fmt_date),
        if matched && got == size {
            "matches"
        } else {
            "DIFFERS"
        },
        t.elapsed()
    );
    Ok(())
}

/// For a stopped upload: how many bytes are there, and are they the file's first bytes?
async fn verify_prefix(ctx: &Ctx, handle: ObjectHandle, file: &Path) {
    match compare(ctx, handle, file).await {
        Ok((matched, got)) => println!(
            "content: {got} bytes, {} the file's first {got} bytes",
            if matched { "equal to" } else { "NOT equal to" }
        ),
        Err(e) => println!("content: read failed: {e}"),
    }
}

async fn compare(ctx: &Ctx, handle: ObjectHandle, file: &Path) -> R<(bool, u64)> {
    let mut f = File::open(file)?;
    let mut stream = ctx.session.get_object_stream(handle).await?;
    let mut got = 0u64;
    let mut matched = true;
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next_chunk().await {
        let chunk = chunk?;
        buf.resize(chunk.len(), 0);
        let n = read_full(&mut f, &mut buf)?;
        if n != chunk.len() || buf[..n] != chunk[..] {
            matched = false;
        }
        got += chunk.len() as u64;
    }
    Ok((matched, got))
}

async fn find(ctx: &Ctx, name: &str) -> R<Option<ObjectHandle>> {
    let handles = ctx
        .session
        .get_object_handles(ctx.storage, None, Some(ctx.download))
        .await?;
    for h in handles {
        if ctx.session.get_object_info(h).await?.filename == name {
            return Ok(Some(h));
        }
    }
    Ok(None)
}

async fn delete(ctx: &Ctx, handle: ObjectHandle) {
    if let Err(e) = ctx.session.delete_object(handle).await {
        println!("  delete {handle:?}: {e}");
    }
}

// ---------------------------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------------------------

async fn open(ext: String, check_every: usize) -> R<Ctx> {
    // Quiet, spaced reopens (docs/debugging.md § 2); never the SIC reset.
    let mut last = None;
    for attempt in 0..5 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        match open_once(ext.clone(), check_every).await {
            Ok(ctx) => return Ok(ctx),
            Err(e) => {
                eprintln!("open attempt {}: {e}", attempt + 1);
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| "open failed".into()))
}

async fn open_once(ext: String, check_every: usize) -> R<Ctx> {
    let device = PtpDevice::open_first().await?;
    // Sets drain-on-cancel for Android, so a stopped data phase ends with a ZLP.
    device.get_device_info().await?;
    let session = Arc::new(device.open_session().await?);
    let storage = *session
        .get_storage_ids()
        .await?
        .first()
        .ok_or("no storage")?;
    let mut download = None;
    for h in session
        .get_object_handles(storage, None, Some(ObjectHandle::ALL))
        .await?
    {
        if session.get_object_info(h).await?.filename == "Download" {
            download = Some(h);
            break;
        }
    }
    Ok(Ctx {
        _device: device,
        session,
        storage,
        download: download.ok_or("no Download folder at the root")?,
        ext,
        check_every,
    })
}

/// Reads `[offset, offset + len)` of `file` in 1 MiB chunks.
fn file_stream(
    file: &Path,
    offset: u64,
    len: u64,
) -> R<impl Stream<Item = std::io::Result<Bytes>> + Unpin + Send> {
    let mut f = File::open(file)?;
    f.seek(SeekFrom::Start(offset))?;
    Ok(Box::pin(stream::unfold(
        (f, len),
        |(mut f, left)| async move {
            if left == 0 {
                return None;
            }
            let n = (left as usize).min(CHUNK);
            let mut buf = vec![0u8; n];
            match read_full(&mut f, &mut buf) {
                Ok(got) if got == n => Some((Ok(Bytes::from(buf)), (f, left - n as u64))),
                Ok(_) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "file too short",
                    )),
                    (f, 0),
                )),
                Err(e) => Some((Err(e), (f, 0))),
            }
        },
    )))
}

/// Passes `inner` through until `limit` bytes have gone out, then fails like a progress `Break`.
fn failing_after(
    inner: impl Stream<Item = std::io::Result<Bytes>> + Unpin + Send,
    limit: u64,
    sent: Arc<AtomicU64>,
) -> impl Stream<Item = std::io::Result<Bytes>> + Unpin + Send {
    inner.map(move |chunk| {
        let chunk = chunk?;
        if sent.load(Ordering::Relaxed) + chunk.len() as u64 > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "stopped",
            ));
        }
        sent.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        Ok(chunk)
    })
}

fn one(b: Bytes) -> impl Stream<Item = std::io::Result<Bytes>> + Unpin + Send {
    stream::iter(vec![Ok(b)])
}

/// Caller latencies with nothing else on the session.
async fn caller_round(ctx: &Ctx, small: ObjectHandle) -> R<[Duration; 3]> {
    let mut r = [Duration::ZERO; 3];
    let t = Instant::now();
    ctx.session
        .get_object_handles(ctx.storage, None, Some(ctx.download))
        .await?;
    r[0] = t.elapsed();
    let t = Instant::now();
    ctx.session.get_object_info(small).await?;
    r[1] = t.elapsed();
    let t = Instant::now();
    ctx.session.get_object(small).await?;
    r[2] = t.elapsed();
    Ok(r)
}

fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn ctx_ext(name: &str) -> String {
    name.rsplit('.').next().unwrap_or("bin").to_string()
}

fn fmt_date(d: DateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        d.year, d.month, d.day, d.hour, d.minute, d.second
    )
}

fn pattern(i: u64) -> u8 {
    (i.wrapping_mul(2_654_435_761) >> 13) as u8
}

fn show<T: std::fmt::Debug>(r: &Result<T, PtpError>) -> String {
    match r {
        Ok(v) => format!("OK {v:?}"),
        Err(e) => format!("ERR {e}"),
    }
}

fn take_opt(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}

fn parse_size(s: &str) -> R<u64> {
    let (num, mul) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1024),
        Some('M' | 'm') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    Ok(num.parse::<u64>()? * mul)
}

fn human(n: u64) -> String {
    if n.is_multiple_of(1024 * 1024) {
        format!("{}M", n / (1024 * 1024))
    } else {
        format!("{}K", n / 1024)
    }
}

fn mbps(bytes: u64, d: Duration) -> f64 {
    bytes as f64 / 1e6 / d.as_secs_f64()
}

fn median(v: &[f64]) -> f64 {
    percentile(v, 0.5)
}

fn percentile(v: &[f64], p: f64) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[((s.len() - 1) as f64 * p).round() as usize]
}
