//! List one folder with and without `GetObjectPropList`, and compare.
//!
//! Lists the folder twice, each in its own session: once with one `GetObjectInfo`
//! per child, once with `GetObjectPropList` (when the device advertises it). Prints
//! both timings and object counts, and every field that differs between the two
//! listings. It only reads.
//!
//! ```text
//! cargo run --release --example list_speed -- DCIM/Camera
//! cargo run --release --example list_speed -- --serial R5CW12345 Pictures/Screenshots
//! cargo run --example list_speed --features virtual-device -- --virtual ~/some/dir sub/folder
//! ```
//!
//! The folder is a `/`-separated path from the first storage's root. `--virtual`
//! serves a local directory through the virtual device instead of real hardware,
//! which checks the comparison, not the speed: its timings say nothing about a
//! real device's.
//! On a device that doesn't advertise `GetObjectPropList` (most cameras), both
//! listings go per handle and take about as long.
//!
//! Expect two differences on purpose: listings via `GetObjectPropList` leave image
//! dimensions at 0, and Android serves no `DateCreated` there, so `created` is
//! `None` where the per-handle listing reports one.

use mtp_rs::mtp::{MtpDevice, Storage};
use mtp_rs::{ObjectHandle, ObjectInfo};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

enum Target {
    First,
    Serial(String),
    #[cfg_attr(not(feature = "virtual-device"), allow(dead_code))]
    Virtual(std::path::PathBuf),
}

const USAGE: &str = "usage: list_speed [--serial SERIAL | --virtual DIR] FOLDER/PATH";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut target = Target::First;
    let mut folder = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--serial" => target = Target::Serial(args.next().ok_or(USAGE)?),
            "--virtual" => target = Target::Virtual(args.next().ok_or(USAGE)?.into()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if folder.is_none() => folder = Some(arg),
            _ => return Err(USAGE.into()),
        }
    }
    let folder = folder.ok_or(USAGE)?;

    let per_handle = list_once(&target, &folder, false).await?;
    let prop_list = list_once(&target, &folder, true).await?;

    println!();
    println!(
        "per handle:        {:>7} objects in {:.2?}",
        per_handle.objects.len(),
        per_handle.elapsed
    );
    println!(
        "GetObjectPropList: {:>7} objects in {:.2?}",
        prop_list.objects.len(),
        prop_list.elapsed
    );
    println!(
        "speedup:           {:.1}x",
        per_handle.elapsed.as_secs_f64() / prop_list.elapsed.as_secs_f64().max(1e-6)
    );

    let differences = compare(&per_handle, &prop_list);
    if differences.is_empty() {
        println!("\nNo differences.");
    } else {
        println!("\n{} difference(s):", differences.len());
        for difference in &differences {
            println!("  {difference}");
        }
    }
    Ok(())
}

/// One session's listing of the folder.
struct Listing {
    folder: ObjectHandle,
    objects: Vec<ObjectInfo>,
    elapsed: Duration,
}

async fn list_once(
    target: &Target,
    path: &str,
    use_prop_list: bool,
) -> Result<Listing, Box<dyn std::error::Error>> {
    let device = open(target, use_prop_list).await?;
    let storages = device.storages().await?;
    let storage = storages.first().ok_or("the device reports no storages")?;
    let folder = resolve(storage, path)
        .await?
        .ok_or_else(|| format!("no folder at {path}"))?;
    if !use_prop_list {
        // Asks only GetObjectHandles: warms the device's directory cache, so the timed
        // listing isn't charged for the device's first scan of the folder.
        drop(storage.list_objects_stream(Some(folder)).await);
    }

    let started = Instant::now();
    let objects = storage.list_objects(Some(folder)).await?;
    let elapsed = started.elapsed();
    drop(storages);
    device.close().await.ok();
    Ok(Listing {
        folder,
        objects,
        elapsed,
    })
}

async fn open(target: &Target, use_prop_list: bool) -> Result<MtpDevice, mtp_rs::Error> {
    let builder = MtpDevice::builder().use_object_prop_list(use_prop_list);
    match target {
        Target::First => builder.open_first().await,
        Target::Serial(serial) => builder.open_by_serial(serial).await,
        #[cfg(feature = "virtual-device")]
        Target::Virtual(dir) => {
            use mtp_rs::{VirtualDeviceConfig, VirtualStorageConfig};
            builder
                .open_virtual(VirtualDeviceConfig {
                    serial: "list-speed".into(),
                    storages: vec![VirtualStorageConfig {
                        description: "Internal Storage".into(),
                        capacity: 1 << 40,
                        backing_dir: dir.clone(),
                        read_only: true,
                    }],
                    event_poll_interval: Duration::ZERO,
                    watch_backing_dirs: false,
                    ..Default::default()
                })
                .await
        }
        #[cfg(not(feature = "virtual-device"))]
        Target::Virtual(_) => {
            eprintln!("--virtual needs: cargo run --example list_speed --features virtual-device");
            Err(mtp_rs::Error::Unsupported)
        }
    }
}

/// Resolve a `/`-separated folder path from the storage root.
async fn resolve(storage: &Storage, path: &str) -> Result<Option<ObjectHandle>, mtp_rs::Error> {
    let mut parent = None;
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        let objects = storage.list_objects(parent).await?;
        match objects
            .iter()
            .find(|o| o.is_folder() && o.filename == segment)
        {
            Some(folder) => parent = Some(folder.handle),
            None => return Ok(None),
        }
    }
    Ok(parent)
}

/// Every difference between the two listings, matching objects by name (handles are
/// per session).
fn compare(a: &Listing, b: &Listing) -> Vec<String> {
    fn by_name(listing: &Listing) -> BTreeMap<&str, &ObjectInfo> {
        listing
            .objects
            .iter()
            .map(|o| (o.filename.as_str(), o))
            .collect()
    }
    let (xs, ys) = (by_name(a), by_name(b));
    let mut out = Vec::new();
    for name in xs.keys().filter(|n| !ys.contains_key(*n)) {
        out.push(format!("{name}: only per handle"));
    }
    for name in ys.keys().filter(|n| !xs.contains_key(*n)) {
        out.push(format!("{name}: only via GetObjectPropList"));
    }
    for (name, x) in &xs {
        let Some(y) = ys.get(name) else { continue };
        let mut field = |label: &str, left: String, right: String| {
            if left != right {
                out.push(format!("{name}: {label} {left} vs {right}"));
            }
        };
        field("size", x.size.to_string(), y.size.to_string());
        field(
            "format",
            format!("{:?}", x.format),
            format!("{:?}", y.format),
        );
        field(
            "folder",
            x.is_folder().to_string(),
            y.is_folder().to_string(),
        );
        field(
            "parent is the listed folder",
            (x.parent == a.folder).to_string(),
            (y.parent == b.folder).to_string(),
        );
        field(
            "storage",
            format!("{:?}", x.storage_id),
            format!("{:?}", y.storage_id),
        );
        field(
            "modified",
            format!("{:?}", x.modified),
            format!("{:?}", y.modified),
        );
        field(
            "created",
            format!("{:?}", x.created),
            format!("{:?}", y.created),
        );
        field(
            "image size",
            format!("{}x{}", x.image_width, x.image_height),
            format!("{}x{}", y.image_width, y.image_height),
        );
    }
    out
}
